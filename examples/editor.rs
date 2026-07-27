//! A minimal notepad: the editor surface, dogfooded.
//!
//! This is the hardest consumer path — identity-keyed shaping over a rope,
//! caret/selection geometry read back from `measure`, wrap affinity, cluster
//! -true stepping — exercised end to end in a few hundred lines. What it shows:
//!
//! - **The consumer owns the text.** The document is a `ropey::Rope`; the
//!   service sees it only through `ParagraphSource`, and only for lines that
//!   miss the cache. Each line carries a stable `(slot, generation)` identity,
//!   so typing on one line reshapes that line and nothing else.
//! - **One handle feeds hit-testing and rendering.** Click, drag, caret and
//!   selection all read the same `Layout` the renderer draws from.
//! - **Wrap affinity is a discipline.** Every caret placement decides its
//!   visual line (`line_hint`); a byte at a soft break is ambiguous and the
//!   caret is typed (`Caret { byte, line }`) and every motion goes through
//!   `Layout::caret_move`, so the ambiguity cannot be dropped on the floor.
//! - **Left/Right step by caret stops** (`Layout::{next,prev}_caret_stop`), so
//!   the caret can't land inside a ligature or a ZWJ emoji sequence.
//! - **Overlay geometry is the consumer's.** Selection and caret are rects in
//!   the example's own tiny pipeline — the service draws glyphs, nothing else.
//!
//! Not here, on purpose: undo, IME composition, bidi (all parked upstream).
//!
//! Interactive:  `cargo run --example editor [-- <file>] [--font <family>]`
//!     Ctrl+O/S open/save · Ctrl+Shift+S save as · Ctrl+A/C/X/V ·
//!     Ctrl+wheel zoom · wheel scroll · Esc clears selection
//! Headless PNG: `cargo run --example editor -- --dump`   → editor.png

mod common;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ropey::Rope;
use sanscale::{
    Align, BlockKey, Boundaries, Caret, Color, Layout, Motion, ParagraphKey, ParagraphSource,
    Rect, ShapedHandle, Style, TextService, Vec2,
};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use common::font_chain;

// Dark mode. Linear-space colors, matching the service's `Color`.
const BG: wgpu::Color = wgpu::Color { r: 0.011, g: 0.012, b: 0.014, a: 1.0 };
const FG: [f32; 4] = [0.83, 0.85, 0.88, 1.0];
const STATUS_FG: [f32; 4] = [0.45, 0.48, 0.54, 1.0];
const STATUS_BG: [f32; 4] = [0.028, 0.030, 0.036, 1.0];
const SELECTION: [f32; 4] = [0.13, 0.25, 0.55, 0.55];
const CARET: [f32; 4] = [0.95, 0.96, 1.0, 1.0];

const MARGIN: f32 = 14.0;
const STATUS_H: f32 = 26.0;
const PAGE_LINES: usize = 20;

/// Mono first (the notepad default), then emoji + broad fallback so pasted
/// CJK or emoji render instead of boxing. `--font` prepends a family.
const MONO_CHAIN: &[&str] = &[
    "Cascadia Mono", "Consolas", "Menlo", "DejaVu Sans Mono", "Courier New",
    "Segoe UI Emoji", "Apple Color Emoji", "Noto Color Emoji",
    "Segoe UI", "Microsoft YaHei", "Noto Sans CJK SC", "Noto Sans",
];

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mut font: Option<String> = None;
    let mut path: Option<PathBuf> = None;
    let mut dump = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--font" => font = args.next(),
            "--dump" => dump = true,
            other if !other.starts_with("--") => path = Some(PathBuf::from(other)),
            other => eprintln!("unknown flag {other}"),
        }
    }
    if dump {
        dump_png(font.as_deref());
        return;
    }
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop
        .run_app(&mut App { gfx: None, font, path })
        .unwrap();
}

// ---------------------------------------------------------------------------
// Document: a rope plus per-line identity
// ---------------------------------------------------------------------------

/// The consumer-side document. The rope is authoritative; `lines` carries one
/// `(slot, generation)` identity per rope line, spliced in step with edits, so
/// the service's per-paragraph cache invalidates exactly the lines an edit
/// touched — the "consumer owns identity" contract, in miniature.
struct Doc {
    rope: Rope,
    lines: Vec<(u32, u32)>,
    next_slot: u32,
    path: Option<PathBuf>,
    dirty: bool,
}

impl Doc {
    fn from_text(text: &str, path: Option<PathBuf>) -> Self {
        let mut doc = Self {
            rope: Rope::from_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
            lines: Vec::new(),
            next_slot: 0,
            path,
            dirty: false,
        };
        doc.lines = (0..doc.rope.len_lines()).map(|_| doc.fresh()).collect();
        doc
    }

    fn fresh(&mut self) -> (u32, u32) {
        self.next_slot += 1;
        (self.next_slot, 0)
    }

    /// Replace a byte range with `insert`, keeping line identities honest: the
    /// first touched line keeps its slot with a bumped generation (its cache
    /// entry invalidates), lines merged away are dropped, lines created get
    /// fresh slots. Everything outside the touched span keeps its identity and
    /// therefore its cache entry.
    fn replace(&mut self, range: std::ops::Range<usize>, insert: &str) {
        let first = self.rope.byte_to_line(range.start);
        let last = self.rope.byte_to_line(range.end).min(self.lines.len() - 1);
        let start_char = self.rope.byte_to_char(range.start);
        let end_char = self.rope.byte_to_char(range.end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, insert);
        let new_last = self.rope.byte_to_line(range.start + insert.len());
        let (keep_slot, keep_gen) = self.lines[first];
        let replacement: Vec<(u32, u32)> = (first..=new_last)
            .map(|index| {
                if index == first {
                    (keep_slot, keep_gen.wrapping_add(1))
                } else {
                    self.fresh()
                }
            })
            .collect();
        self.lines.splice(first..=last, replacement);
        debug_assert_eq!(self.lines.len(), self.rope.len_lines());
        self.dirty = true;
    }

    fn keys(&self) -> Vec<ParagraphKey> {
        self.lines
            .iter()
            .map(|&(slot, generation)| ParagraphKey { namespace: 1, slot, generation })
            .collect()
    }

    /// A line's byte range, excluding its trailing newline.
    fn line_bytes(&self, index: usize) -> std::ops::Range<usize> {
        let start = self.rope.line_to_byte(index);
        let end = if index + 1 < self.rope.len_lines() {
            self.rope.line_to_byte(index + 1) - 1
        } else {
            self.rope.len_bytes()
        };
        start..end
    }

    fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".into())
    }
}

/// The service pulls a line's text only when that line's `(key, style)` misses
/// the shaping cache — for an unchanged document this is never called at all.
impl ParagraphSource for Doc {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>> {
        let &(slot, generation) = self.lines.get(index)?;
        if key.slot != slot || key.generation != generation {
            return None; // stale identity: skip rather than shape the wrong text
        }
        let slice = self.rope.byte_slice(self.line_bytes(index));
        Some(match slice.as_str() {
            Some(s) => Cow::Borrowed(s),
            None => Cow::Owned(slice.to_string()),
        })
    }
}

// ---------------------------------------------------------------------------
// Editor state: caret, selection, affinity
// ---------------------------------------------------------------------------

struct Editor {
    doc: Doc,
    /// The placed caret: byte **and** visual line, as one value. The library's
    /// `caret_move` keeps the pair honest; `clamp_caret` re-anchors it after a
    /// reshape. The hand-rolled hint bookkeeping this replaces was the part
    /// that kept going wrong.
    caret: Caret,
    anchor: Option<usize>,
    /// Vertical-motion goal column (em) — owned here because the service is
    /// stateless; `caret_move` seeds, preserves and clears it.
    goal: Option<f32>,
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
            caret: Caret { byte_index: 0, line_index: 0 },
            anchor: None,
            goal: None,
            scroll_y: 0.0,
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
        let range = self.doc.rope.byte_to_char(range.start)..self.doc.rope.byte_to_char(range.end);
        Some(self.doc.rope.slice(range).to_string())
    }

    /// Keep the caret inside the viewport after motion or edits.
    fn scroll_caret_into_view(&mut self, layout: &Layout, view_h: f32) {
        let caret = layout.clamp_caret(self.caret);
        let rect = layout.caret_rect_on_line(Some(caret.line_index), caret.byte_index);
        let top = MARGIN + rect.y_em * self.font_px - self.scroll_y;
        let height = (rect.height_em.max(1.0)) * self.font_px;
        if top < MARGIN {
            self.scroll_y -= MARGIN - top;
        } else if top + height > view_h - STATUS_H - MARGIN {
            self.scroll_y += top + height - (view_h - STATUS_H - MARGIN);
        }
        self.scroll_y = self.scroll_y.max(0.0);
    }
}

/// Word boundaries are semantics over the rope, not shaping — the library asks
/// through this seam exactly the way it asks for text through
/// `ParagraphSource`, and never holds the text.
impl Boundaries for Doc {
    fn prev_word(&self, byte: usize) -> Option<usize> {
        let mut chars = self.rope.chars_at(self.rope.byte_to_char(byte));
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
        for ch in self.rope.chars_at(self.rope.byte_to_char(byte)) {
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

// ---------------------------------------------------------------------------
// Rendering: text through the service, overlays through a tiny rect pipeline
// ---------------------------------------------------------------------------

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
            wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
            wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x4 },
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
        Self { pipeline, verts: Vec::new() }
    }

    fn push(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4], screen: Vec2) {
        let ndc = |px: f32, py: f32| {
            [px / screen.x * 2.0 - 1.0, 1.0 - py / screen.y * 2.0]
        };
        let [x0, y0] = ndc(x, y);
        let [x1, y1] = ndc(x + w, y + h);
        let v = |x: f32, y: f32| [x, y, color[0], color[1], color[2], color[3]];
        self.verts.extend([
            v(x0, y0), v(x1, y0), v(x1, y1),
            v(x0, y0), v(x1, y1), v(x0, y1),
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

/// Everything one frame needs. Shape → measure → overlays → draw, all against
/// the same handle, which is what keeps hit-testing and pixels in agreement.
struct Frame {
    handle: Option<ShapedHandle>,
}

#[allow(clippy::too_many_arguments)]
fn render_frame(
    text: &mut TextService,
    rects: &mut RectPainter,
    editor: &mut Editor,
    style: &Style,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pass: &mut wgpu::RenderPass<'_>,
    screen: Vec2,
    caret_visible: bool,
) -> Frame {
    let keys = editor.doc.keys();
    let handle = text.shape(BlockKey(1), style, &keys, &editor.doc);
    let Some(handle) = handle else {
        return Frame { handle: None };
    };

    let font_px = editor.font_px;
    let origin = Vec2::new(MARGIN, MARGIN - editor.scroll_y);
    let view = Rect::new(0.0, 0.0, screen.x, screen.y - STATUS_H);

    // Overlays first (under the glyphs): selection spans, then the status bar
    // strip; the caret goes on top of the text at the end.
    {
        let layout = text.measure(handle);
        if let Some(range) = editor.selection() {
            for span in layout.selection(range) {
                rects.push(
                    origin.x + span.x_em * font_px,
                    origin.y + span.y_em * font_px,
                    (span.width_em * font_px).max(2.0),
                    span.height_em * font_px,
                    SELECTION,
                    screen,
                );
            }
        }
    }
    rects.push(0.0, screen.y - STATUS_H, screen.x, STATUS_H, STATUS_BG, screen);
    rects.flush(device, pass);

    text.draw(
        device,
        queue,
        pass,
        handle,
        origin,
        font_px,
        Color(FG),
        Some(view),
    );

    // Status line: name, dirty marker, caret position. Content-keyed transient
    // shaping — no identity to invent for chrome text.
    let status = {
        let layout = text.measure(handle);
        let caret = layout.clamp_caret(editor.caret);
        let line = caret.line_index;
        let col = layout
            .line_range(line)
            .map(|r| {
                editor
                    .doc
                    .rope
                    .byte_slice(r.start..caret.byte_index.max(r.start).min(r.end))
                    .len_chars()
            })
            .unwrap_or(0);
        format!(
            "{}{}   ·   Ln {}, Col {}   ·   {:.0} px   ·   Ctrl+O open · Ctrl+S save",
            editor.doc.name(),
            if editor.doc.dirty { " •" } else { "" },
            line + 1,
            col + 1,
            font_px,
        )
    };
    if let Some(chrome) = text.shape_transient(&status, style) {
        text.draw(
            device,
            queue,
            pass,
            chrome,
            Vec2::new(MARGIN, screen.y - STATUS_H + 5.0),
            13.0,
            Color(STATUS_FG),
            None,
        );
    }

    // Caret, over the glyphs. The block form covers the next cluster (its
    // width read from the caret stops), translucent so the glyph shows through.
    if caret_visible {
        let layout = text.measure(handle);
        let placed = layout.clamp_caret(editor.caret);
        let line = Some(placed.line_index);
        let caret = layout.caret_rect_on_line(line, placed.byte_index);
        let height = if caret.height_em > 0.0 { caret.height_em } else { 1.2 };
        if editor.caret_block {
            let width_em = line
                .and_then(|l| layout.line_range(l))
                .zip(layout.next_caret_stop(placed.byte_index))
                .filter(|(range, next)| *next <= range.end)
                .map(|(_, next)| {
                    (layout.caret_rect_on_line(line, next).x_em - caret.x_em).abs()
                })
                .filter(|w| *w > 0.05)
                .unwrap_or(0.55);
            let mut color = CARET;
            color[3] = 0.45;
            rects.push(
                origin.x + caret.x_em * font_px,
                origin.y + caret.y_em * font_px,
                width_em * font_px,
                height * font_px,
                color,
                screen,
            );
        } else {
            rects.push(
                origin.x + caret.x_em * font_px - 0.5,
                origin.y + caret.y_em * font_px,
                1.5,
                height * font_px,
                CARET,
                screen,
            );
        }
    }
    rects.flush(device, pass);
    Frame { handle: Some(handle) }
}

// ---------------------------------------------------------------------------
// Interactive window
// ---------------------------------------------------------------------------

struct App {
    gfx: Option<Gfx>,
    font: Option<String>,
    path: Option<PathBuf>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            self.gfx = Some(pollster::block_on(Gfx::new(
                event_loop,
                self.font.as_deref(),
                self.path.take(),
            )));
        }
    }

    /// Blink scheduling: wake at the next phase flip and repaint only when one
    /// actually happened — no continuous redraw loop.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        let since = gfx.last_input.elapsed().as_millis() as u64;
        let phase = since / BLINK_MS;
        if phase != gfx.blink_phase {
            gfx.blink_phase = phase;
            gfx.window.request_redraw();
        }
        let next_flip = BLINK_MS - (since % BLINK_MS);
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(next_flip.max(1)),
        ));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(mods) => gfx.mods = mods.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                gfx.on_key(event);
            }
            WindowEvent::Resized(size) => {
                gfx.resize(size);
                gfx.window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                if gfx.mods.control_key() {
                    gfx.editor.font_px = (gfx.editor.font_px * 1.1f32.powf(dy)).clamp(5.0, 160.0);
                } else {
                    gfx.editor.scroll_y =
                        (gfx.editor.scroll_y - dy * gfx.editor.font_px * 3.0).max(0.0);
                }
                gfx.window.request_redraw();
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    gfx.dragging = true;
                    gfx.place_at_cursor(gfx.mods.shift_key());
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
    chain: sanscale::FontChainHandle,
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
}

/// Half a blink cycle: visible for one period, hidden for the next.
const BLINK_MS: u64 = 530;

impl Gfx {
    async fn new(event_loop: &ActiveEventLoop, font: Option<&str>, path: Option<PathBuf>) -> Self {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("sanscale editor")
                        .with_inner_size(PhysicalSize::new(900, 640)),
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
                label: Some("editor"),
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
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
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
        let chain = font_chain(&mut text, &chain_families(font));
        let rects = RectPainter::new(&device, format);

        let doc = match &path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(content) => Doc::from_text(&content, path.clone()),
                Err(e) => {
                    eprintln!("open {}: {e}", p.display());
                    Doc::from_text("", None)
                }
            },
            None => Doc::from_text("", None),
        };

        let gfx = Self {
            window,
            surface,
            config,
            device,
            queue,
            text,
            rects,
            chain,
            editor: Editor::new(doc),
            last_handle: None,
            mods: ModifiersState::empty(),
            cursor: Vec2::new(0.0, 0.0),
            dragging: false,
            last_input: Instant::now(),
            blink_phase: 0,
        };
        gfx.update_title();
        gfx.window.request_redraw();
        gfx
    }

    fn style(&self) -> Style {
        let wrap = (self.config.width as f32 - 2.0 * MARGIN) / self.editor.font_px;
        Style {
            chain: self.chain,
            wrap_em: Some(wrap.max(4.0)),
            align: Align::Left,
            line_spacing: 1.15,
        }
    }

    fn update_title(&self) {
        self.window.set_title(&format!(
            "{}{} — sanscale editor",
            self.editor.doc.name(),
            if self.editor.doc.dirty { " •" } else { "" },
        ));
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Mouse → byte, through the same layout the last frame drew. `hit_test`
    /// answers with both the byte *and* the visual line, which is the affinity
    /// hint — a click near a soft break lands on the line you clicked.
    fn place_at_cursor(&mut self, select: bool) {
        let Some(handle) = self.last_handle else { return };
        let layout = self.text.measure(handle);
        let em = Vec2::new(
            (self.cursor.x - MARGIN) / self.editor.font_px,
            (self.cursor.y - MARGIN + self.editor.scroll_y) / self.editor.font_px,
        );
        if let Some(hit) = layout.hit_test(em) {
            self.last_input = Instant::now();
            self.blink_phase = 0;
            self.editor.place(hit, select);
            self.editor.goal = None;
            self.window.request_redraw();
        }
    }

    fn on_key(&mut self, event: winit::event::KeyEvent) {
        self.last_input = Instant::now();
        self.blink_phase = 0;
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();
        let Some(handle) = self.last_handle else { return };
        // The borrow dance every consumer does: clone nothing, take the layout
        // queries you need while `&self.text` is shared, mutate after.
        let editor = &mut self.editor;
        let text = &self.text;
        let layout = text.measure(handle);

        let mut edited = true;
        match event.logical_key {
            Key::Named(NamedKey::ArrowLeft) if ctrl => editor.motion(layout, Motion::WordLeft, shift),
            Key::Named(NamedKey::ArrowRight) if ctrl => editor.motion(layout, Motion::WordRight, shift),
            Key::Named(NamedKey::ArrowLeft) => editor.motion(layout, Motion::Left, shift),
            Key::Named(NamedKey::ArrowRight) => editor.motion(layout, Motion::Right, shift),
            Key::Named(NamedKey::ArrowUp) => editor.motion(layout, Motion::Up, shift),
            Key::Named(NamedKey::ArrowDown) => editor.motion(layout, Motion::Down, shift),
            Key::Named(NamedKey::Home) if ctrl => editor.motion(layout, Motion::DocStart, shift),
            Key::Named(NamedKey::End) if ctrl => editor.motion(layout, Motion::DocEnd, shift),
            Key::Named(NamedKey::Home) => editor.motion(layout, Motion::Home, shift),
            Key::Named(NamedKey::End) => editor.motion(layout, Motion::End, shift),
            Key::Named(NamedKey::PageUp) => editor.motion(layout, Motion::PageUp(PAGE_LINES), shift),
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
                        byte_index: editor.doc.rope.len_bytes(),
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
            if let Some(handle) = self.text.shape(BlockKey(1), &style, &keys, &self.editor.doc) {
                self.last_handle = Some(handle);
                let view_h = self.config.height as f32;
                let layout = self.text.measure(handle);
                self.editor.settle(layout);
                self.editor.scroll_caret_into_view(layout, view_h);
            }
        }
        self.update_title();
        self.window.request_redraw();
    }

    fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("text", &["txt", "md", "rs", "toml", "log"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.editor = Editor::new(Doc::from_text(&content, Some(path)));
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
        match std::fs::write(&path, self.editor.doc.rope.to_string()) {
            Ok(()) => self.editor.doc.dirty = false,
            Err(e) => eprintln!("save {}: {e}", path.display()),
        }
    }

    fn draw(&mut self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let screen = Vec2::new(self.config.width as f32, self.config.height as f32);

        self.text.set_target(&self.device, self.config.format);
        self.text.set_transform(
            &self.queue,
            TextService::pixel_ortho(self.config.width, self.config.height),
        );

        let style = self.style();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor"),
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
            let caret_visible =
                (self.last_input.elapsed().as_millis() as u64 / BLINK_MS) % 2 == 0;
            let result = render_frame(
                &mut self.text,
                &mut self.rects,
                &mut self.editor,
                &style,
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

fn chain_families(extra: Option<&str>) -> Vec<&str> {
    let mut families = Vec::new();
    if let Some(name) = extra {
        families.push(name);
    }
    families.extend_from_slice(MONO_CHAIN);
    families
}

// ---------------------------------------------------------------------------
// Headless dump: one composed frame, no window
// ---------------------------------------------------------------------------

const DUMP_TEXT: &str = "The sanscale editor — a notepad in a few hundred lines.\n\nEverything on screen goes through one ShapedHandle: this text, the caret,\nthe selection you see highlighted here, and the status bar all read the same\nlayout, so hit-testing and pixels cannot disagree.\n\nWrap affinity, cluster stepping (try 👨‍👩‍👧‍👦 or ﬁ), CJK fallback 你好世界,\nand identity-keyed shaping: editing one line reshapes one line.\n";

fn dump_png(font: Option<&str>) {
    let harness = common::Harness::new(900, 620);
    let mut text = TextService::new();
    let chain = font_chain(&mut text, &chain_families(font));
    let mut rects = RectPainter::new(&harness.device, harness.config.format);
    let mut editor = Editor::new(Doc::from_text(DUMP_TEXT, None));
    // A selection spanning the wrap on the third paragraph, and the caret at
    // its end — the dump shows overlays, not just glyphs.
    // Spanning the blank line between two paragraphs, so the dump proves the
    // newline stubs render (a blank line inside a selection is not invisible).
    let start = DUMP_TEXT.find("a notepad").unwrap();
    let end = DUMP_TEXT.find("disagree").unwrap();
    editor.anchor = Some(start);
    editor.caret = Caret { byte_index: end, line_index: usize::MAX }; // clamped at render
    let style = Style {
        chain,
        wrap_em: Some((900.0 - 2.0 * MARGIN) / editor.font_px),
        align: Align::Left,
        line_spacing: 1.15,
    };
    harness.save_png(&mut text, BG, "editor.png", |text, device, queue, pass| {
        render_frame(
            text,
            &mut rects,
            &mut editor,
            &style,
            device,
            queue,
            pass,
            Vec2::new(900.0, 620.0),
            true,
        );
    });
    println!("wrote editor.png");
}
