//! Split-view source editing and live Markdown preview. The custom parser is
//! window/GPU independent; `preview` is a separate sanscale adapter. Neither is
//! added to the main crate's runtime API or dependencies.
//! Run `cargo run --release --example markdown-editor -- [file.md]`.
//! F2 palette, F3 italic faces, F4 reveal source block in preview, F5 append/pause
//! a simulated agent message, F6 follow tail. Wheel scrolls the pane under the pointer; Shift+wheel pans wide
//! tables; Ctrl+wheel zooms. Click preview text to place the source caret.
//! `--dump` writes markdown-editor.png; `--dump --stream` exercises chunked
//! appends before writing markdown-editor-stream.png. No GUI in headless mode.
mod common;
#[path = "markdown-editor/fonts.rs"]
mod fonts;
#[allow(dead_code)]
#[path = "markdown-editor/markdown/mod.rs"]
mod markdown;
#[path = "markdown-editor/preview.rs"]
mod preview;
#[path = "markdown-editor/probe.rs"]
mod probe;
use preview::{Faces, Preview, Scene, Theme};
use sanscale::{
    Align, Batch, BlockKey, Boundaries, Caret, Color, Draw, Layout, Motion, ParagraphKey,
    ParagraphSource, Rect, ShapedHandle, Style, TextService, Vec2,
};
use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};
const BG: wgpu::Color = wgpu::Color {
    r: 0.010,
    g: 0.014,
    b: 0.020,
    a: 1.,
};
const FG: [f32; 4] = [0.70, 0.76, 0.83, 1.];
const STATUS_FG: [f32; 4] = [0.40, 0.49, 0.58, 1.];
const STATUS_BG: [f32; 4] = [0.019, 0.030, 0.046, 1.];
const SELECTION: [f32; 4] = [0.09, 0.24, 0.44, 0.7];
const CARET: [f32; 4] = [0.45, 0.86, 0.94, 1.];
const MARGIN: f32 = 18.;
const HEADER_H: f32 = 42.;
const STATUS_H: f32 = 28.;
const PAGE_LINES: usize = 20;
const DUMP_TEXT: &str = include_str!("markdown-editor/sample.md");
const STREAM_TEXT: &str = include_str!("markdown-editor/stream.md");
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mut font = None;
    let mut path = None;
    let mut dump = false;
    let mut stream = false;
    let mut bench = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--font" => font = args.next(),
            "--dump" => dump = true,
            "--stream" => stream = true,
            "--bench" => bench = true,
            other if !other.starts_with("--") => path = Some(PathBuf::from(other)),
            other => eprintln!("unknown flag {other}"),
        }
    }
    if bench {
        probe::run(font.as_deref());
        return;
    }
    if dump {
        dump_png(font.as_deref(), path.as_deref(), stream);
        return;
    }
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop
        .run_app(&mut App {
            gfx: None,
            font,
            path,
            stream,
        })
        .unwrap();
}
static NAMESPACE: AtomicU32 = AtomicU32::new(1000);
struct Doc {
    md: markdown::Document,
    namespace: u32,
    path: Option<PathBuf>,
    dirty: bool,
}
impl Doc {
    fn from_text(text: &str, path: Option<PathBuf>) -> Self {
        Self {
            md: markdown::Document::new(&text.replace("\r\n", "\n").replace('\r', "\n")),
            namespace: NAMESPACE.fetch_add(2, Ordering::Relaxed),
            path,
            dirty: false,
        }
    }
    fn replace(&mut self, range: std::ops::Range<usize>, insert: &str) {
        let old = self.md.revision();
        self.md
            .edit(range, insert)
            .expect("editor edits UTF-8 boundaries");
        self.dirty |= self.md.revision() != old;
    }
    fn keys(&self) -> Vec<ParagraphKey> {
        (0..self.md.line_count())
            .map(|i| {
                let (slot, generation) = self.md.line_key(i).unwrap();
                ParagraphKey {
                    namespace: self.namespace as u64,
                    slot,
                    generation,
                }
            })
            .collect()
    }
    fn name(&self) -> String {
        self.path.as_ref().and_then(|p| p.file_name()).map_or_else(
            || "untitled.md".into(),
            |s| s.to_string_lossy().into_owned(),
        )
    }
}
impl ParagraphSource for Doc {
    fn paragraph_text(&self, i: usize, key: ParagraphKey) -> Option<Cow<'_, str>> {
        if key.namespace != self.namespace as u64
            || self.md.line_key(i) != Some((key.slot, key.generation))
        {
            return None;
        }
        self.md.line_text(i).map(Cow::Borrowed)
    }
}
struct Panes {
    source: Rect,
    preview: Rect,
    split: f32,
}
fn panes(screen: Vec2) -> Panes {
    let split = (screen.x * 0.46).floor();
    let y = HEADER_H + MARGIN;
    let h = (screen.y - y - STATUS_H - MARGIN).max(1.);
    Panes {
        source: Rect::new(MARGIN, y, (split - 2. * MARGIN).max(1.), h),
        preview: Rect::new(
            split + MARGIN,
            y,
            (screen.x - split - 2. * MARGIN).max(1.),
            h,
        ),
        split,
    }
}
struct Playback {
    offset: usize,
    next: Instant,
    running: bool,
}
impl Playback {
    fn new() -> Self {
        Self {
            offset: 0,
            next: Instant::now(),
            running: true,
        }
    }
    fn next_chunk(&mut self) -> String {
        let rest = &STREAM_TEXT[self.offset..];
        let bytes = rest.chars().take(14).map(char::len_utf8).sum::<usize>();
        let chunk = rest[..bytes].to_owned();
        self.offset += bytes;
        self.next = Instant::now() + Duration::from_millis(45);
        chunk
    }
}
struct Editor {
    doc: Doc,
    alternate_palette: bool,
    italic_preview: bool,
    /// The placed caret: byte **and** visual line, as one value. The library's
    /// `caret_move` keeps the pair honest; `clamp_caret` re-anchors it after a
    /// reshape. The hand-rolled hint bookkeeping this replaces was the part
    /// that kept going wrong.
    caret: Caret,
    anchor: Option<usize>,
    /// Vertical-motion goal column (em) — owned here because the service is
    /// stateless; `caret_move` seeds, preserves and clears it.
    goal: Option<f32>,
    preview_scroll: Vec2,
    follow_tail: bool,
    scroll_y: f32, // px
    font_px: f32,
    /// Insert toggles between the bar caret and a block (overtype-style) caret
    /// covering the next cluster.
    caret_block: bool,
}

impl Editor {
    fn new(doc: Doc) -> Self {
        Self {
            doc,
            alternate_palette: false,
            italic_preview: true,
            caret: Caret {
                byte_index: 0,
                line_index: 0,
            },
            anchor: None,
            goal: None,
            scroll_y: 0.0,
            preview_scroll: Vec2::new(0., 0.),
            follow_tail: false,
            font_px: 17.0,
            caret_block: false,
        }
    }

    fn selection(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.anchor?;
        let byte = self.caret.byte_index;
        let (a, b) = (anchor.min(byte), anchor.max(byte));
        (a != b).then_some(a..b)
    }

    /// Move the caret, extending or collapsing the selection. Does not touch
    /// `goal` — `caret_move` owns its lifecycle; other placement paths (mouse,
    /// edits) clear it themselves.
    fn place(&mut self, caret: Caret, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret.byte_index);
            }
        } else {
            self.anchor = None;
        }
        self.caret = caret;
    }

    /// One library call per keypress. The affinity rules, boundary snaps and
    /// goal-column lifecycle all live in [`Layout::caret_move`] now — this
    /// method replaced ~80 lines of the bookkeeping that kept going wrong.
    fn motion(&mut self, layout: &Layout, motion: Motion, select: bool) {
        let next = layout.caret_move(self.caret, motion, &mut self.goal, &self.doc);
        self.place(next, select);
    }

    /// After an edit, resolve the caret against the fresh layout — end-affine
    /// at a soft break, so typing the character that wraps stays on its line.
    fn settle(&mut self, layout: &Layout) {
        self.caret = layout.caret_after_edit(self.caret.byte_index);
    }

    fn insert(&mut self, text: &str) {
        let byte = self.caret.byte_index;
        let range = self.selection().unwrap_or(byte..byte);
        let at = range.start;
        self.doc.replace(range, text);
        self.edit_placed(at + text.len());
    }

    /// Backspace/Delete step by caret stops too — one keypress removes one
    /// cluster, so a ZWJ emoji family goes as a unit instead of decomposing.
    fn backspace(&mut self, layout: &Layout) {
        let byte = self.caret.byte_index;
        let range = match self.selection() {
            Some(range) => range,
            None => match layout.prev_caret_stop(byte) {
                Some(prev) => prev..byte,
                None => return,
            },
        };
        let at = range.start;
        self.doc.replace(range, "");
        self.edit_placed(at);
    }

    fn delete(&mut self, layout: &Layout) {
        let byte = self.caret.byte_index;
        let range = match self.selection() {
            Some(range) => range,
            None => match layout.next_caret_stop(byte) {
                Some(next) => byte..next,
                None => return,
            },
        };
        let at = range.start;
        self.doc.replace(range, "");
        self.edit_placed(at);
    }

    /// Post-edit caret: the line index is stale until the reshape (`settle`
    /// runs then); byte is authoritative now.
    fn edit_placed(&mut self, byte: usize) {
        self.caret.byte_index = byte;
        self.anchor = None;
        self.goal = None;
    }

    fn selected_text(&self) -> Option<String> {
        let range = self.selection()?;
        let range = self.doc.md.source().byte_to_char(range.start)
            ..self.doc.md.source().byte_to_char(range.end);
        Some(self.doc.md.source().slice(range).to_string())
    }

    /// Keep the caret inside the viewport after motion or edits.
    fn scroll_caret_into_view(&mut self, layout: &Layout, view_h: f32) {
        let caret = layout.clamp_caret(self.caret);
        let rect = layout.caret_rect_on_line(Some(caret.line_index), caret.byte_index);
        let top = MARGIN + rect.y_em * self.font_px - self.scroll_y;
        let height = (rect.height_em.max(1.0)) * self.font_px;
        if top < MARGIN {
            self.scroll_y -= MARGIN - top;
        } else if top + height > view_h - MARGIN {
            self.scroll_y += top + height - (view_h - MARGIN);
        }
        self.scroll_y = self.scroll_y.max(0.0);
    }
}

/// Word boundaries are semantics over the rope, not shaping — the library asks
/// through this seam exactly the way it asks for text through
/// `ParagraphSource`, and never holds the text.
impl Boundaries for Doc {
    fn prev_word(&self, byte: usize) -> Option<usize> {
        let mut chars = self
            .md
            .source()
            .chars_at(self.md.source().byte_to_char(byte));
        let mut offset = byte;
        let mut in_word = false;
        while let Some(ch) = chars.prev() {
            if in_word && !ch.is_alphanumeric() && ch != '_' {
                break;
            }
            if ch.is_alphanumeric() || ch == '_' {
                in_word = true;
            }
            offset -= ch.len_utf8();
            if in_word && offset == 0 {
                break;
            }
        }
        Some(offset)
    }

    fn next_word(&self, byte: usize) -> Option<usize> {
        let mut offset = byte;
        let mut in_word = false;
        for ch in self
            .md
            .source()
            .chars_at(self.md.source().byte_to_char(byte))
        {
            if in_word && !(ch.is_alphanumeric() || ch == '_') {
                break;
            }
            if ch.is_alphanumeric() || ch == '_' {
                in_word = true;
            }
            offset += ch.len_utf8();
        }
        Some(offset)
    }
}

const RECT_SHADER: &str = "
struct VsOut { @builtin(position) pos: vec4f, @location(0) color: vec4f }
@vertex
fn vs(@location(0) pos: vec2f, @location(1) color: vec4f) -> VsOut {
    var out: VsOut;
    out.pos = vec4f(pos, 0.0, 1.0);
    out.color = color;
    return out;
}
@fragment
fn fs(in: VsOut) -> @location(0) vec4f { return in.color; }
";

/// Solid rects in NDC — selection, caret, status bar. The service deliberately
/// draws glyphs and nothing else; overlay geometry is the consumer's.
struct RectPainter {
    pipeline: wgpu::RenderPipeline,
    verts: Vec<[f32; 6]>,
}

impl RectPainter {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rects"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rects"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let attrs = [
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x4,
            },
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rects"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self {
            pipeline,
            verts: Vec::new(),
        }
    }

    fn push(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4], screen: Vec2) {
        let ndc = |px: f32, py: f32| [px / screen.x * 2.0 - 1.0, 1.0 - py / screen.y * 2.0];
        let [x0, y0] = ndc(x, y);
        let [x1, y1] = ndc(x + w, y + h);
        let v = |x: f32, y: f32| [x, y, color[0], color[1], color[2], color[3]];
        self.verts.extend([
            v(x0, y0),
            v(x1, y0),
            v(x1, y1),
            v(x0, y0),
            v(x1, y1),
            v(x0, y1),
        ]);
    }

    fn flush(&mut self, device: &wgpu::Device, pass: &mut wgpu::RenderPass<'_>) {
        if self.verts.is_empty() {
            return;
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rects"),
            contents: bytemuck::cast_slice(&self.verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..self.verts.len() as u32, 0..1);
        self.verts.clear();
    }
}

#[derive(Default)]
struct CachedBatch {
    draws: Vec<Draw>,
    batch: Option<Batch>,
}
impl CachedBatch {
    fn prepare(
        &mut self,
        text: &mut TextService,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        draws: &[Draw],
    ) {
        if self.draws != draws || self.batch.as_ref().is_none_or(|b| !text.batch_live(b)) {
            self.batch = Some(text.prepare(device, queue, draws));
            self.draws = draws.to_vec();
        }
    }
    fn draw(&self, text: &TextService, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(b) = &self.batch {
            text.draw_prepared(pass, b);
        }
    }
}
struct RenderCache {
    source: CachedBatch,
    rendered: CachedBatch,
    chrome: CachedBatch,
    preview: Preview,
    scene: Scene,
}
impl RenderCache {
    fn new(namespace: u32) -> Self {
        Self {
            source: CachedBatch::default(),
            rendered: CachedBatch::default(),
            chrome: CachedBatch::default(),
            preview: Preview::new(namespace),
            scene: Scene::default(),
        }
    }
}
struct Frame {
    handle: Option<ShapedHandle>,
}
fn scissor(pass: &mut wgpu::RenderPass<'_>, rect: Rect, screen: Vec2) {
    let x = rect.x.max(0.).floor().min(screen.x - 1.) as u32;
    let y = rect.y.max(0.).floor().min(screen.y - 1.) as u32;
    let right = (rect.x + rect.width)
        .ceil()
        .min(screen.x)
        .max(x as f32 + 1.) as u32;
    let bottom = (rect.y + rect.height)
        .ceil()
        .min(screen.y)
        .max(y as f32 + 1.) as u32;
    pass.set_scissor_rect(x, y, right - x, bottom - y);
}
fn render_frame(
    text: &mut TextService,
    rects: &mut RectPainter,
    cache: &mut RenderCache,
    editor: &mut Editor,
    faces: Faces,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pass: &mut wgpu::RenderPass<'_>,
    screen: Vec2,
    caret_visible: bool,
) -> Frame {
    let panes = panes(screen);
    let theme = Theme {
        alternate: editor.alternate_palette,
        italic: editor.italic_preview,
    };
    let style = Style {
        chain: faces.mono[0],
        wrap_em: Some((panes.source.width / editor.font_px).max(1.)),
        align: Align::Left,
        line_spacing: 1.15,
    };
    let Some(handle) = text.shape(BlockKey(1), &style, &editor.doc.keys(), &editor.doc) else {
        return Frame { handle: None };
    };
    cache.preview.sync(
        &editor.doc.md,
        text,
        faces,
        theme,
        panes.preview.width,
        editor.font_px,
    );
    editor.scroll_y = editor.scroll_y.min(
        (text.measure(handle).height_em() * editor.font_px - panes.source.height + MARGIN).max(0.),
    );
    if editor.follow_tail {
        editor.preview_scroll.y = (cache.preview.height - panes.preview.height).max(0.);
    }
    editor.preview_scroll.y = editor
        .preview_scroll
        .y
        .clamp(0., (cache.preview.height - panes.preview.height).max(0.));
    editor.preview_scroll.x = editor
        .preview_scroll
        .x
        .clamp(0., (cache.preview.width - panes.preview.width).max(0.));
    cache.scene = cache
        .preview
        .scene(text, &editor.doc.md, panes.preview, editor.preview_scroll);
    let source_origin = Vec2::new(panes.source.x, panes.source.y - editor.scroll_y);
    let source_draw = Draw {
        block: handle,
        at: source_origin,
        size: editor.font_px,
        color: Color(FG),
        clip: Some(panes.source),
        ..Default::default()
    };
    let placed = text.measure(handle).clamp_caret(editor.caret);
    let line = editor.doc.md.source().byte_to_line(placed.byte_index);
    let col = editor
        .doc
        .md
        .source()
        .byte_slice(editor.doc.md.source().line_to_byte(line)..placed.byte_index)
        .len_chars();
    let p = editor.doc.md.last_change().work;
    let w = cache.preview.last_work;
    let status = format!(
        "Ln {}:{}  ·  last edit: {} lines parsed / {} projected  ·  {} layout requests  ·  F2 palette  F3 italic  F5 stream/pause  F6 follow {}",
        line + 1,
        col + 1,
        p.classified_lines,
        p.projected_elements,
        w.layout_requests,
        if editor.follow_tail { "on" } else { "off" }
    );
    let title = format!(
        "MARKDOWN   /   {}{}",
        editor.doc.name(),
        if editor.doc.dirty { " •" } else { "" }
    );
    let mut chrome = Vec::new();
    let ui_style = Style {
        chain: faces.mono[0],
        wrap_em: None,
        align: Align::Left,
        line_spacing: 1.,
    };
    for (s, at, size, color) in [
        (title, Vec2::new(MARGIN, 13.), 12., Color(STATUS_FG)),
        (
            "LIVE PREVIEW   /   click text to locate source".into(),
            Vec2::new(panes.split + MARGIN, 13.),
            12.,
            theme.accent(),
        ),
        (
            status,
            Vec2::new(MARGIN, screen.y - STATUS_H + 7.),
            11.,
            Color(STATUS_FG),
        ),
    ] {
        if let Some(block) = text.shape_transient(&s, &ui_style) {
            chrome.push(Draw {
                block,
                at,
                size,
                color,
                ..Default::default()
            });
        }
    }
    // All shaping and all prepares happen before any glyph draw binds atlases.
    cache.source.prepare(text, device, queue, &[source_draw]);
    cache
        .rendered
        .prepare(text, device, queue, &cache.scene.draws);
    cache.chrome.prepare(text, device, queue, &chrome);
    pass.set_scissor_rect(0, 0, screen.x as u32, screen.y as u32);
    rects.push(
        panes.split,
        HEADER_H,
        screen.x - panes.split,
        screen.y - HEADER_H - STATUS_H,
        [0.013, 0.019, 0.028, 1.],
        screen,
    );
    rects.push(0., 0., screen.x, HEADER_H, STATUS_BG, screen);
    rects.push(
        0.,
        screen.y - STATUS_H,
        screen.x,
        STATUS_H,
        STATUS_BG,
        screen,
    );
    rects.push(
        panes.split - 1.,
        0.,
        1.,
        screen.y,
        [0.07, 0.12, 0.18, 1.],
        screen,
    );
    rects.flush(device, pass);
    scissor(pass, panes.source, screen);
    if let Some(range) = editor.selection() {
        for s in text.measure(handle).selection(range) {
            rects.push(
                source_origin.x + s.x_em * editor.font_px,
                source_origin.y + s.y_em * editor.font_px,
                (s.width_em * editor.font_px).max(2.),
                s.height_em * editor.font_px,
                SELECTION,
                screen,
            );
        }
    }
    rects.flush(device, pass);
    cache.source.draw(text, pass);
    if caret_visible {
        let c = text
            .measure(handle)
            .caret_rect_on_line(Some(placed.line_index), placed.byte_index);
        let width = if editor.caret_block {
            editor.font_px * 0.6
        } else {
            1.5
        };
        let mut color = CARET;
        if editor.caret_block {
            color[3] = 0.4;
        }
        rects.push(
            source_origin.x + c.x_em * editor.font_px - 0.5,
            source_origin.y + c.y_em * editor.font_px,
            width,
            c.height_em * editor.font_px,
            color,
            screen,
        );
        rects.flush(device, pass);
    }
    scissor(pass, panes.preview, screen);
    for d in &cache.scene.under {
        let r = d.rect;
        rects.push(r.x, r.y, r.width, r.height, d.color.0, screen);
    }
    rects.flush(device, pass);
    cache.rendered.draw(text, pass);
    for d in &cache.scene.over {
        let r = d.rect;
        rects.push(r.x, r.y, r.width, r.height, d.color.0, screen);
    }
    rects.flush(device, pass);
    pass.set_scissor_rect(0, 0, screen.x as u32, screen.y as u32);
    cache.chrome.draw(text, pass);
    Frame {
        handle: Some(handle),
    }
}

struct App {
    gfx: Option<Gfx>,
    font: Option<String>,
    path: Option<PathBuf>,
    stream: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            self.gfx = Some(pollster::block_on(Gfx::new(
                event_loop,
                self.font.as_deref(),
                self.path.take(),
            )));
            if self.stream {
                let gfx = self.gfx.as_mut().unwrap();
                gfx.playback = Some(Playback::new());
                gfx.editor.follow_tail = true;
            }
        }
    }

    /// Blink scheduling: wake at the next phase flip and repaint only when one
    /// actually happened — no continuous redraw loop.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        gfx.advance_stream();
        let since = gfx.last_input.elapsed().as_millis() as u64;
        let phase = since / BLINK_MS;
        if phase != gfx.blink_phase {
            gfx.blink_phase = phase;
            gfx.window.request_redraw();
        }
        let next_flip = BLINK_MS - (since % BLINK_MS);
        let mut next = Instant::now() + Duration::from_millis(next_flip.max(1));
        if let Some(p) = &gfx.playback {
            if p.running {
                next = next.min(p.next);
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(next));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(mods) => gfx.mods = mods.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                gfx.on_key(event);
            }
            WindowEvent::Resized(_) => gfx.window.request_redraw(),
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                let panes = panes(Vec2::new(gfx.config.width as f32, gfx.config.height as f32));
                if gfx.mods.control_key() {
                    gfx.editor.font_px = (gfx.editor.font_px * 1.1f32.powf(dy)).clamp(8., 48.);
                } else if gfx.cursor.x >= panes.split {
                    gfx.editor.follow_tail = false;
                    if gfx.mods.shift_key() {
                        gfx.editor.preview_scroll.x =
                            (gfx.editor.preview_scroll.x - dy * 40.).max(0.);
                    } else {
                        gfx.editor.preview_scroll.y =
                            (gfx.editor.preview_scroll.y - dy * gfx.editor.font_px * 3.).max(0.);
                    }
                } else {
                    gfx.editor.follow_tail = false;
                    gfx.editor.scroll_y =
                        (gfx.editor.scroll_y - dy * gfx.editor.font_px * 3.).max(0.);
                }
                gfx.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => {
                    gfx.editor.follow_tail = false;
                    let panes = panes(Vec2::new(gfx.config.width as f32, gfx.config.height as f32));
                    if gfx.cursor.x >= panes.split {
                        if gfx.cursor.y < panes.preview.y
                            || gfx.cursor.y >= panes.preview.y + panes.preview.height
                        {
                            return;
                        }
                        if let Some(byte) = gfx.cache.preview.hit_source(
                            &gfx.cache.scene,
                            gfx.cursor,
                            &gfx.text,
                            &gfx.editor.doc.md,
                        ) {
                            gfx.editor.caret.byte_index = byte;
                            gfx.editor.caret.line_index = usize::MAX;
                            gfx.editor.anchor = None;
                            gfx.refresh_source();
                            gfx.last_input = Instant::now();
                            gfx.window.request_redraw();
                        }
                        return;
                    }
                    let now = Instant::now();
                    let count = match gfx.last_click {
                        Some((t, p, c))
                            if now.duration_since(t).as_millis() < 400
                                && (gfx.cursor.x - p.x).abs() + (gfx.cursor.y - p.y).abs()
                                    < 6.0 =>
                        {
                            c + 1
                        }
                        _ => 1,
                    };
                    gfx.last_click = Some((now, gfx.cursor, count));
                    gfx.dragging = count == 1;
                    match count {
                        1 => gfx.place_at_cursor(gfx.mods.shift_key()),
                        2 => gfx.select_word_at_cursor(),
                        _ => {
                            gfx.select_paragraph_at_cursor();
                            gfx.last_click = None; // a 4th click starts over
                        }
                    }
                }
                ElementState::Released => gfx.dragging = false,
            },
            WindowEvent::CursorMoved { position, .. } => {
                gfx.cursor = Vec2::new(position.x as f32, position.y as f32);
                if gfx.dragging {
                    gfx.place_at_cursor(true);
                }
            }
            WindowEvent::RedrawRequested => gfx.draw(),
            _ => {}
        }
    }
}

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    text: TextService,
    rects: RectPainter,
    fonts: Faces,
    cache: RenderCache,
    playback: Option<Playback>,
    editor: Editor,
    /// The layout the last frame drew from — hit-testing reads the same handle.
    last_handle: Option<ShapedHandle>,
    mods: ModifiersState,
    cursor: Vec2,
    dragging: bool,
    /// Caret blink anchor: any input resets it, so the caret is solid while
    /// you type and blinks only at rest.
    last_input: Instant,
    blink_phase: u64,
    /// Previous left-press + running click count, for double/triple-click
    /// detection (winit doesn't count clicks; that is consumer work — only the
    /// *range* each tier selects comes from the library).
    last_click: Option<(Instant, Vec2, u32)>,
}

/// Half a blink cycle: visible for one period, hidden for the next.
const BLINK_MS: u64 = 530;

impl Gfx {
    async fn new(event_loop: &ActiveEventLoop, font: Option<&str>, path: Option<PathBuf>) -> Self {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("sanscale markdown-editor")
                        .with_inner_size(PhysicalSize::new(1480, 940)),
                )
                .unwrap(),
        );
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await
            .expect("adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("markdown-editor"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .expect("device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let mut text = TextService::new();
        let fonts = fonts::load(&mut text, font);
        let rects = RectPainter::new(&device, format);

        let doc = match &path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(content) => Doc::from_text(&content, path.clone()),
                Err(e) => {
                    eprintln!("open {}: {e}", p.display());
                    Doc::from_text("", None)
                }
            },
            None => Doc::from_text(DUMP_TEXT, None),
        };

        let gfx = Self {
            window,
            surface,
            config,
            device,
            queue,
            text,
            rects,
            fonts,
            cache: RenderCache::new(doc.namespace + 1),
            playback: None,
            editor: Editor::new(doc),
            last_handle: None,
            mods: ModifiersState::empty(),
            cursor: Vec2::new(0.0, 0.0),
            dragging: false,
            last_input: Instant::now(),
            blink_phase: 0,
            last_click: None,
        };
        gfx.update_title();
        gfx.window.request_redraw();
        gfx
    }

    fn style(&self) -> Style {
        let wrap = panes(Vec2::new(
            self.config.width as f32,
            self.config.height as f32,
        ))
        .source
        .width
            / self.editor.font_px;
        Style {
            chain: self.fonts.mono[0],
            wrap_em: Some(wrap.max(1.0)),
            align: Align::Left,
            line_spacing: 1.15,
        }
    }

    fn update_title(&self) {
        self.window.set_title(&format!(
            "{}{} — sanscale markdown-editor",
            self.editor.doc.name(),
            if self.editor.doc.dirty { " •" } else { "" },
        ));
    }

    /// Mouse → placed caret, through the same layout the last frame drew.
    /// `hit_test` answers with both the byte *and* the visual line, which is
    /// the affinity — a click near a soft break lands on the line you clicked.
    fn hit_at_cursor(&self) -> Option<Caret> {
        let panes = panes(Vec2::new(
            self.config.width as f32,
            self.config.height as f32,
        ));
        if self.cursor.x >= panes.split
            || self.cursor.y < HEADER_H
            || self.cursor.y >= self.config.height as f32 - STATUS_H
        {
            return None;
        }
        let handle = self.last_handle?;
        let layout = self.text.measure(handle);
        let em = Vec2::new(
            (self.cursor.x - MARGIN) / self.editor.font_px,
            (self.cursor.y - HEADER_H - MARGIN + self.editor.scroll_y) / self.editor.font_px,
        );
        layout.hit_test(em)
    }

    fn place_at_cursor(&mut self, select: bool) {
        if let Some(hit) = self.hit_at_cursor() {
            self.last_input = Instant::now();
            self.blink_phase = 0;
            self.editor.place(hit, select);
            self.editor.goal = None;
            self.window.request_redraw();
        }
    }

    /// Double-click word selection: the library composes the same `Boundaries`
    /// the word motions use ([`Layout::select_word_at`]).
    fn select_word_at_cursor(&mut self) {
        let Some(hit) = self.hit_at_cursor() else {
            return;
        };
        let Some(handle) = self.last_handle else {
            return;
        };
        let layout = self.text.measure(handle);
        let range = layout.select_word_at(hit.byte_index, &self.editor.doc);
        self.select_range(range);
    }

    /// Triple-click paragraph selection — pure geometry, hard break to hard
    /// break ([`Layout::select_paragraph_at`]).
    fn select_paragraph_at_cursor(&mut self) {
        let Some(hit) = self.hit_at_cursor() else {
            return;
        };
        let Some(handle) = self.last_handle else {
            return;
        };
        let layout = self.text.measure(handle);
        let range = layout.select_paragraph_at(hit.byte_index);
        self.select_range(range);
    }

    fn select_range(&mut self, range: std::ops::Range<usize>) {
        let Some(handle) = self.last_handle else {
            return;
        };
        let layout = self.text.measure(handle);
        let caret = layout.caret_at(range.end);
        self.last_input = Instant::now();
        self.blink_phase = 0;
        self.editor.anchor = Some(range.start);
        self.editor.caret = caret;
        self.editor.goal = None;
        self.window.request_redraw();
    }

    fn refresh_source(&mut self) {
        let style = self.style();
        let keys = self.editor.doc.keys();
        if let Some(handle) = self
            .text
            .shape(BlockKey(1), &style, &keys, &self.editor.doc)
        {
            self.last_handle = Some(handle);
            let layout = self.text.measure(handle);
            self.editor.caret = layout.clamp_caret(self.editor.caret);
            self.editor.scroll_caret_into_view(
                layout,
                panes(Vec2::new(
                    self.config.width as f32,
                    self.config.height as f32,
                ))
                .source
                .height,
            );
        }
    }
    fn advance_stream(&mut self) {
        let Some(p) = &mut self.playback else {
            return;
        };
        if !p.running || Instant::now() < p.next {
            return;
        }
        let chunk = p.next_chunk();
        let end = self.editor.doc.md.source().len_bytes();
        self.editor.doc.replace(end..end, &chunk);
        if self.editor.follow_tail {
            self.editor.caret.byte_index = self.editor.doc.md.source().len_bytes();
            self.editor.caret.line_index = usize::MAX;
            self.editor.anchor = None;
            self.refresh_source();
        }
        if self
            .playback
            .as_ref()
            .is_some_and(|p| p.offset == STREAM_TEXT.len())
        {
            self.playback = None;
        }
        self.update_title();
        self.window.request_redraw();
    }
    fn on_key(&mut self, event: winit::event::KeyEvent) {
        self.last_input = Instant::now();
        self.blink_phase = 0;
        if event.logical_key == Key::Named(NamedKey::F4) {
            if let Some(id) = self
                .editor
                .doc
                .md
                .block_at_source(self.editor.caret.byte_index)
            {
                if let Some(y) = self.cache.preview.block_y(id) {
                    self.editor.preview_scroll.y = y;
                    self.editor.follow_tail = false;
                }
            }
            self.window.request_redraw();
            return;
        }
        if event.logical_key == Key::Named(NamedKey::F5) {
            if let Some(p) = &mut self.playback {
                p.running = !p.running;
                p.next = Instant::now();
            } else {
                self.playback = Some(Playback::new());
            }
            self.editor.follow_tail = true;
            self.window.request_redraw();
            return;
        }
        if event.logical_key == Key::Named(NamedKey::F6) {
            self.editor.follow_tail = !self.editor.follow_tail;
            self.window.request_redraw();
            return;
        }
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();
        let Some(handle) = self.last_handle else {
            return;
        };
        // The borrow dance every consumer does: clone nothing, take the layout
        // queries you need while `&self.text` is shared, mutate after.
        let editor = &mut self.editor;
        let text = &self.text;
        let layout = text.measure(handle);

        let previous_revision = editor.doc.md.revision();
        let mut edited = true;
        match event.logical_key {
            Key::Named(NamedKey::F2) => {
                editor.alternate_palette = !editor.alternate_palette;
                edited = false;
            }
            Key::Named(NamedKey::F3) => {
                editor.italic_preview = !editor.italic_preview;
                editor.goal = None;
            }
            Key::Named(NamedKey::ArrowLeft) if ctrl => {
                editor.motion(layout, Motion::WordLeft, shift)
            }
            Key::Named(NamedKey::ArrowRight) if ctrl => {
                editor.motion(layout, Motion::WordRight, shift)
            }
            Key::Named(NamedKey::ArrowLeft) => editor.motion(layout, Motion::Left, shift),
            Key::Named(NamedKey::ArrowRight) => editor.motion(layout, Motion::Right, shift),
            Key::Named(NamedKey::ArrowUp) => editor.motion(layout, Motion::Up, shift),
            Key::Named(NamedKey::ArrowDown) => editor.motion(layout, Motion::Down, shift),
            Key::Named(NamedKey::Home) if ctrl => editor.motion(layout, Motion::DocStart, shift),
            Key::Named(NamedKey::End) if ctrl => editor.motion(layout, Motion::DocEnd, shift),
            Key::Named(NamedKey::Home) => editor.motion(layout, Motion::Home, shift),
            Key::Named(NamedKey::End) => editor.motion(layout, Motion::End, shift),
            Key::Named(NamedKey::PageUp) => {
                editor.motion(layout, Motion::PageUp(PAGE_LINES), shift)
            }
            Key::Named(NamedKey::PageDown) => {
                editor.motion(layout, Motion::PageDown(PAGE_LINES), shift)
            }
            Key::Named(NamedKey::Backspace) => editor.backspace(layout),
            Key::Named(NamedKey::Delete) => editor.delete(layout),
            Key::Named(NamedKey::Enter) => editor.insert("\n"),
            Key::Named(NamedKey::Tab) => editor.insert("    "),
            Key::Named(NamedKey::Escape) => editor.anchor = None,
            Key::Named(NamedKey::Insert) => {
                editor.caret_block = !editor.caret_block;
                edited = false;
            }
            Key::Character(ref c) if ctrl => match c.as_str() {
                "a" | "A" => {
                    editor.anchor = Some(0);
                    editor.caret = Caret {
                        byte_index: editor.doc.md.source().len_bytes(),
                        line_index: usize::MAX, // clamped at next use
                    };
                    editor.goal = None;
                }
                "c" | "C" => {
                    if let Some(s) = editor.selected_text() {
                        common::copy_to_clipboard(&s);
                    }
                    edited = false;
                }
                "x" | "X" => {
                    if let Some(s) = editor.selected_text() {
                        common::copy_to_clipboard(&s);
                        editor.backspace(layout);
                    }
                }
                "v" | "V" => {
                    if let Ok(s) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                        editor.insert(&s.replace("\r\n", "\n").replace('\r', "\n"));
                    }
                }
                "s" | "S" => {
                    self.save(shift);
                    edited = false;
                }
                "o" | "O" => {
                    self.open();
                    edited = false;
                }
                _ => edited = false,
            },
            _ => match &event.text {
                Some(t) if !ctrl && t.chars().all(|c| !c.is_control()) => editor.insert(t),
                _ => edited = false,
            },
        }
        if edited {
            // Reshape now so ensure-visible sees post-edit geometry, then keep
            // the caret on screen. `&mut self.text` and `&mut self.editor` are
            // disjoint fields, so the layout borrow and the editor mutation
            // coexist — the same shape compendium's port proved out.
            let keys = self.editor.doc.keys();
            let style = self.style();
            if let Some(handle) = self
                .text
                .shape(BlockKey(1), &style, &keys, &self.editor.doc)
            {
                self.last_handle = Some(handle);
                let view_h = panes(Vec2::new(
                    self.config.width as f32,
                    self.config.height as f32,
                ))
                .source
                .height;
                let layout = self.text.measure(handle);
                if self.editor.doc.md.revision() != previous_revision {
                    if let Some(p) = &mut self.playback {
                        p.running = false;
                    }
                    self.editor.settle(layout);
                } else {
                    self.editor.caret = layout.clamp_caret(self.editor.caret);
                }
                self.editor.scroll_caret_into_view(layout, view_h);
            }
        }
        self.update_title();
        self.window.request_redraw();
    }

    fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Markdown", &["md", "markdown"])
            .add_filter("text", &["txt", "md", "rs", "toml", "log"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.cache.preview.release(&mut self.text);
                self.editor = Editor::new(Doc::from_text(&content, Some(path)));
                self.cache = RenderCache::new(self.editor.doc.namespace + 1);
                self.playback = None;
                self.last_handle = None;
            }
            Err(e) => eprintln!("open {}: {e}", path.display()),
        }
    }

    fn save(&mut self, save_as: bool) {
        let path = if save_as || self.editor.doc.path.is_none() {
            let Some(p) = rfd::FileDialog::new()
                .set_file_name(self.editor.doc.name())
                .save_file()
            else {
                return;
            };
            self.editor.doc.path = Some(p.clone());
            p
        } else {
            self.editor.doc.path.clone().expect("checked above")
        };
        match std::fs::write(&path, self.editor.doc.md.source().to_string()) {
            Ok(()) => self.editor.doc.dirty = false,
            Err(e) => eprintln!("save {}: {e}", path.display()),
        }
    }

    fn draw(&mut self) {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        if self.config.width != size.width || self.config.height != size.height {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
        let mut acquired = self.surface.get_current_texture();
        if matches!(
            &acquired,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost
        ) {
            self.surface.configure(&self.device, &self.config);
            acquired = self.surface.get_current_texture();
        }
        let frame = match acquired {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let screen = Vec2::new(self.config.width as f32, self.config.height as f32);

        self.text.set_target(&self.device, self.config.format);
        self.text.set_transform(
            &self.queue,
            TextService::pixel_ortho(self.config.width, self.config.height),
        );

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("markdown-editor"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(BG),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let caret_visible = (self.last_input.elapsed().as_millis() as u64 / BLINK_MS) & 1 == 0;
            let result = render_frame(
                &mut self.text,
                &mut self.rects,
                &mut self.cache,
                &mut self.editor,
                self.fonts,
                &self.device,
                &self.queue,
                &mut pass,
                screen,
                caret_visible,
            );
            self.last_handle = result.handle;
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
    }
}

fn dump_png(font: Option<&str>, path: Option<&std::path::Path>, stream: bool) {
    let harness = common::Harness::new(1480, 940);
    let mut text = TextService::new();
    let faces = fonts::load(&mut text, font);
    let mut rects = RectPainter::new(&harness.device, harness.config.format);
    let content = path.map(|p| {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("open {}: {e}", p.display()))
    });
    let mut editor = Editor::new(Doc::from_text(
        content.as_deref().unwrap_or(DUMP_TEXT),
        Some(
            path.map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("message.md")),
        ),
    ));
    let mut cache = RenderCache::new(editor.doc.namespace + 1);
    if stream {
        let mut playback = Playback::new();
        let mut totals = markdown::Work::default();
        while playback.offset < STREAM_TEXT.len() {
            let chunk = playback.next_chunk();
            let end = editor.doc.md.source().len_bytes();
            editor.doc.replace(end..end, &chunk);
            let w = editor.doc.md.last_change().work;
            totals.classified_lines += w.classified_lines;
            totals.projected_elements += w.projected_elements;
            totals.projected_bytes += w.projected_bytes;
            totals.reused_elements += w.reused_elements;
            totals.metadata_lines += w.metadata_lines;
            cache.preview.sync(
                &editor.doc.md,
                &mut text,
                faces,
                Theme::default(),
                panes(Vec2::new(1480., 940.)).preview.width,
                editor.font_px,
            );
        }
        editor.follow_tail = true;
        editor.caret.byte_index = editor.doc.md.source().len_bytes();
        editor.caret.line_index = usize::MAX;
        let style = Style {
            chain: faces.mono[0],
            wrap_em: Some(panes(Vec2::new(1480., 940.)).source.width / editor.font_px),
            align: Align::Left,
            line_spacing: 1.15,
        };
        let h = text
            .shape(BlockKey(1), &style, &editor.doc.keys(), &editor.doc)
            .unwrap();
        editor.scroll_caret_into_view(text.measure(h), panes(Vec2::new(1480., 940.)).source.height);
        println!(
            "streamed {} UTF-8 bytes; adapter work: {totals:?}",
            STREAM_TEXT.len()
        );
    } else if path.is_none() {
        let start = DUMP_TEXT.find("stream-friendly").unwrap();
        editor.anchor = Some(start);
        editor.caret = Caret {
            byte_index: start + "stream-friendly".len(),
            line_index: usize::MAX,
        };
    }
    let output = if stream {
        "markdown-editor-stream.png"
    } else {
        "markdown-editor.png"
    };
    harness.save_png(&mut text, BG, output, |text, device, queue, pass| {
        render_frame(
            text,
            &mut rects,
            &mut cache,
            &mut editor,
            faces,
            device,
            queue,
            pass,
            Vec2::new(1480., 940.),
            true,
        );
    });
    #[cfg(feature = "perf-counters")]
    {
        sanscale::profiling::reset_work_counters();
        // Same input, next completed GPU frame: text batches must be retained.
        harness.save_png(&mut text, BG, output, |text, device, queue, pass| {
            render_frame(
                text,
                &mut rects,
                &mut cache,
                &mut editor,
                faces,
                device,
                queue,
                pass,
                Vec2::new(1480., 940.),
                false, // hide the caret without changing text draws
            );
        });
        let c = sanscale::profiling::work_counters();
        assert_eq!(
            (
                c.shape_calls,
                c.flow_calls,
                c.prepares,
                c.vertex_upload_bytes
            ),
            (0, 0, 0, 0)
        );
        assert!(
            c.text_draw_calls > 0,
            "headless check must execute glyph draws"
        );
        println!("retained-frame check: zero shaping/flow/prepares/text-vertex uploads");
    }
    cache.preview.release(&mut text);
    println!("wrote {output}");
}
