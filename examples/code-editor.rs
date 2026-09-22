//! A small C editor: syntax paint and real inline bold/italic faces.
//!
//! This is the hardest consumer path — identity-keyed shaping over a rope,
//! caret/selection geometry read back from `measure`, wrap affinity, cluster
//! -true stepping — exercised end to end in a few hundred lines. What it shows:
//!
//! - **The consumer owns the text.** The document is a `ropey::Rope`; the
//!   service sees it only through `ParagraphSource`, and only for lines that
//!   miss the cache. Each line carries a stable `(slot, generation)` identity,
//!   so ordinary typing reshapes that line, not every line. A block-comment
//!   edit can propagate lexical state and effective font changes down the file.
//!   Block assembly still copies all paragraphs (see performance.md).
//! - **Lexing and themes are separate.** C tokens are cached per physical line;
//!   re-lexing stops when incoming state rejoins the unchanged suffix. F2 changes
//!   only paint; F3 resolves comment fonts without reparsing or dirtying the file.
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
//! Interactive:  `cargo run --example code-editor [-- <file>] [--font <family>]`
//!     Ctrl+O/S open/save · Ctrl+Shift+S save as · Ctrl+A/C/X/V ·
//!     Ctrl+wheel zoom · wheel scroll · F2 palette · F3 italic comments · Esc clears selection
//! Headless PNG: `cargo run --example code-editor -- --dump [file.c]` → code-editor.png
//! The built-in ring-buffer sample opens when no file is supplied. This is a
//! lexical C demo, not a complete preprocessor, language server, or IDE.

mod common;
#[path = "code-editor/fonts.rs"]
mod fonts;
#[path = "code-editor/syntax.rs"]
mod syntax;
use fonts::CodeFonts;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use ropey::Rope;
use sanscale::{
    Align, Batch, BlockKey, Boundaries, Caret, Color, Draw, FontSpan, Layout, Motion, PaintHandle,
    PaintSpan, ParagraphKey, ParagraphSource, Rect, ShapedHandle, Style, TextService, Vec2,
};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

// Dark mode. Linear-space colors, matching the service's `Color`.
const BG: wgpu::Color = wgpu::Color {
    r: 0.011,
    g: 0.012,
    b: 0.014,
    a: 1.0,
};
const FG: [f32; 4] = [0.83, 0.85, 0.88, 1.0];
const STATUS_FG: [f32; 4] = [0.45, 0.48, 0.54, 1.0];
const STATUS_BG: [f32; 4] = [0.028, 0.030, 0.036, 1.0];
const SELECTION: [f32; 4] = [0.13, 0.25, 0.55, 0.55];
const CARET: [f32; 4] = [0.95, 0.96, 1.0, 1.0];

const MARGIN: f32 = 14.0;
const STATUS_H: f32 = 26.0;
const PAGE_LINES: usize = 20;

/// Mono first (the code-editor default), then emoji + broad fallback so pasted
/// CJK or emoji render instead of boxing. `--font` prepends a family.
const MONO_CHAIN: &[&str] = &[
    "Cascadia Mono",
    "Consolas",
    "Menlo",
    "DejaVu Sans Mono",
    "Courier New",
    "Segoe UI Emoji",
    "Apple Color Emoji",
    "Noto Color Emoji",
    "Segoe UI",
    "Microsoft YaHei",
    "Noto Sans CJK SC",
    "Noto Sans",
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
        dump_png(font.as_deref(), path.as_deref());
        return;
    }
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop
        .run_app(&mut App {
            gfx: None,
            font,
            path,
        })
        .unwrap();
}

// ---------------------------------------------------------------------------
// Document: a rope plus per-line identity
// ---------------------------------------------------------------------------

/// The consumer-side document. The rope is authoritative; `lines` carries one
/// `(slot, generation)` identity per rope line, spliced in step with edits, so
/// the service's per-paragraph cache invalidates exactly the lines an edit
/// touched — the "consumer owns identity" contract, in miniature.
struct Line {
    slot: u32,
    generation: u32,
    valid: bool,
    incoming: syntax::State,
    outgoing: syntax::State,
    tokens: Vec<syntax::Token>,
    fonts: Vec<FontSpan>,
    style_dirty: bool,
}
impl Line {
    fn new(slot: u32, generation: u32) -> Self {
        Self {
            slot,
            generation,
            valid: false,
            incoming: syntax::State::Code,
            outgoing: syntax::State::Code,
            tokens: Vec::new(),
            fonts: Vec::new(),
            style_dirty: true,
        }
    }
}
struct Doc {
    rope: Rope,
    lines: Vec<Line>,
    namespace: u64,
    next_slot: u32,
    revision: u64,
    styles_pending: bool,
    italic_theme: Option<bool>,
    // Adapter work counter: theme changes must not call the lexer.
    lexed_lines: usize,
    path: Option<PathBuf>,
    dirty: bool,
}
static DOCUMENT_NAMESPACE: AtomicU64 = AtomicU64::new(1);

impl Doc {
    fn from_text(text: &str, path: Option<PathBuf>) -> Self {
        let mut doc = Self {
            rope: Rope::from_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
            lines: Vec::new(),
            next_slot: 0,
            namespace: DOCUMENT_NAMESPACE.fetch_add(1, Ordering::Relaxed),
            revision: 0,
            styles_pending: true,
            italic_theme: None,
            lexed_lines: 0,
            path,
            dirty: false,
        };
        doc.lines = (0..doc.rope.len_lines()).map(|_| doc.fresh()).collect();
        doc.relex(0, doc.lines.len() - 1);
        doc
    }

    fn fresh(&mut self) -> Line {
        self.next_slot += 1;
        Line::new(self.next_slot, 0)
    }

    /// Replace a byte range with `insert`, keeping line identities honest: the
    /// first touched line keeps its slot with a bumped generation (its cache
    /// entry invalidates), lines merged away are dropped, lines created get
    /// fresh slots. Everything outside the touched span keeps its identity and
    /// therefore its cache entry.
    fn replace(&mut self, range: std::ops::Range<usize>, insert: &str) {
        if self.rope.byte_slice(range.clone()) == insert {
            return;
        }
        let first = self.rope.byte_to_line(range.start);
        let last = self.rope.byte_to_line(range.end).min(self.lines.len() - 1);
        let start_char = self.rope.byte_to_char(range.start);
        let end_char = self.rope.byte_to_char(range.end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, insert);
        let new_last = self.rope.byte_to_line(range.start + insert.len());
        let (keep_slot, keep_gen) = (self.lines[first].slot, self.lines[first].generation);
        let replacement: Vec<Line> = (first..=new_last)
            .map(|index| {
                if index == first {
                    Line::new(keep_slot, keep_gen.wrapping_add(1))
                } else {
                    self.fresh()
                }
            })
            .collect();
        self.lines.splice(first..=last, replacement);
        debug_assert_eq!(self.lines.len(), self.rope.len_lines());
        self.dirty = true;
        self.revision += 1;
        self.styles_pending = true;
        self.relex(first, new_last);
    }

    /// Re-lex changed physical lines, then propagate only until the saved
    /// incoming state agrees again. Text/line identity outside the edit survives.
    fn relex(&mut self, first: usize, forced_last: usize) {
        let mut state = if first == 0 {
            syntax::State::Code
        } else {
            self.lines[first - 1].outgoing
        };
        for index in first..self.lines.len() {
            let old = &self.lines[index];
            if index > forced_last && old.valid && old.incoming == state {
                break;
            }
            let slice = self.rope.byte_slice(self.line_bytes(index));
            let content = slice
                .as_str()
                .map(Cow::Borrowed)
                .unwrap_or_else(|| Cow::Owned(slice.to_string()));
            let (tokens, next) = syntax::lex(&content, state);
            self.lexed_lines += 1;
            let row = &mut self.lines[index];
            if row.tokens != tokens {
                row.style_dirty = true;
                self.styles_pending = true;
            }
            row.tokens = tokens;
            row.incoming = state;
            row.outgoing = next;
            row.valid = true;
            state = next;
        }
    }

    /// Theme resolution is independent of lexing, file dirtiness, and source
    /// revision. Only changed effective font spans bump shaping generations.
    fn resolve_fonts(&mut self, fonts: CodeFonts, italic_comments: bool) {
        let theme_changed = self.italic_theme != Some(italic_comments);
        if !theme_changed && !self.styles_pending {
            return;
        }
        for index in 0..self.lines.len() {
            if !theme_changed && !self.lines[index].style_dirty {
                continue;
            }
            let slice = self.rope.byte_slice(self.line_bytes(index));
            let content = slice
                .as_str()
                .map(Cow::Borrowed)
                .unwrap_or_else(|| Cow::Owned(slice.to_string()));
            let resolved = syntax::fonts(
                &content,
                &self.lines[index].tokens,
                fonts.normal,
                fonts.bold,
                fonts.italic,
                italic_comments,
            );
            let row = &mut self.lines[index];
            if row.fonts != resolved {
                row.fonts = resolved;
                row.generation = row.generation.wrapping_add(1);
            }
            row.style_dirty = false;
        }
        self.styles_pending = false;
        self.italic_theme = Some(italic_comments);
    }

    /// Paint uses composed-block bytes. Rebuilding this flat snapshot visits
    /// all tokens on an edit/theme change, not on caret motion or blink.
    fn paint_spans(&self, alternate: bool) -> Vec<PaintSpan> {
        let mut spans: Vec<PaintSpan> = Vec::new();
        let mut offset = 0;
        for (row, slice) in self.lines.iter().zip(self.rope.lines()) {
            for token in &row.tokens {
                let color = syntax::color(token.kind, alternate);
                let range = token.range.start + offset..token.range.end + offset;
                if let Some(last) = spans
                    .last_mut()
                    .filter(|v| v.color == color && v.range.end == range.start)
                {
                    last.range.end = range.end;
                } else {
                    spans.push(PaintSpan { range, color });
                }
            }
            offset += slice.len_bytes();
        }
        spans
    }

    fn keys(&self) -> Vec<ParagraphKey> {
        self.lines
            .iter()
            .map(|line| ParagraphKey {
                namespace: self.namespace,
                slot: line.slot,
                generation: line.generation,
            })
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
            .unwrap_or_else(|| "untitled.c".into())
    }
}

/// The service pulls a line's text only when that line's `(key, style)` misses
/// the shaping cache — for an unchanged document this is never called at all.
impl ParagraphSource for Doc {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>> {
        let row = self.lines.get(index)?;
        if key.namespace != self.namespace
            || key.slot != row.slot
            || key.generation != row.generation
        {
            return None; // stale identity: skip rather than shape the wrong text
        }
        let slice = self.rope.byte_slice(self.line_bytes(index));
        Some(match slice.as_str() {
            Some(s) => Cow::Borrowed(s),
            None => Cow::Owned(slice.to_string()),
        })
    }
    fn paragraph_fonts(&self, index: usize, key: ParagraphKey) -> Cow<'_, [FontSpan]> {
        match self.lines.get(index) {
            Some(row)
                if key.namespace == self.namespace
                    && key.slot == row.slot
                    && key.generation == row.generation =>
            {
                Cow::Borrowed(&row.fonts)
            }
            _ => Cow::Borrowed(&[]),
        }
    }
}

// ---------------------------------------------------------------------------
// Editor state: caret, selection, affinity
// ---------------------------------------------------------------------------

struct Editor {
    doc: Doc,
    alternate_palette: bool,
    italic_comments: bool,
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
            alternate_palette: false,
            italic_comments: true,
            caret: Caret {
                byte_index: 0,
                line_index: 0,
            },
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

/// Consumer-owned retention. A caret blink changes neither draw inputs nor
/// layout revisions, so neither text batch is uploaded again.
#[derive(Default)]
struct CachedBatch {
    draw: Option<Draw>,
    batch: Option<Batch>,
}
impl CachedBatch {
    fn prepare(
        &mut self,
        text: &mut TextService,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        draw: Draw,
    ) {
        if self.draw != Some(draw)
            || self
                .batch
                .as_ref()
                .is_none_or(|batch| !text.batch_live(batch))
        {
            self.batch = Some(text.prepare(device, queue, &[draw]));
            self.draw = Some(draw);
        }
    }
    fn draw(&self, text: &TextService, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(batch) = &self.batch {
            text.draw_prepared(pass, batch);
        }
    }
}
#[derive(Default)]
struct RenderCache {
    body: CachedBatch,
    chrome: CachedBatch,
    paint: Option<PaintHandle>,
    spans: Vec<PaintSpan>,
    paint_input: Option<(u64, u64, bool)>,
}
impl RenderCache {
    fn paint(&mut self, text: &mut TextService, doc: &Doc, alternate: bool) -> Option<PaintHandle> {
        let input = (doc.namespace, doc.revision, alternate);
        if self.paint_input != Some(input) {
            let spans = doc.paint_spans(alternate);
            if spans != self.spans {
                let next = if spans.is_empty() {
                    None
                } else {
                    Some(text.register_paint(&spans).expect("editor paint snapshot"))
                };
                if let Some(old) = self.paint.take() {
                    text.drop_paint(old);
                }
                self.paint = next;
                self.spans = spans;
            }
            self.paint_input = Some(input);
        }
        self.paint
    }
}
struct Frame {
    handle: Option<ShapedHandle>,
}

/// Prepare all text/atlases before recording any glyph draw. Every interaction
/// still reads the very same shaped layout as rendering, never token rectangles.
fn render_frame(
    text: &mut TextService,
    rects: &mut RectPainter,
    cache: &mut RenderCache,
    editor: &mut Editor,
    fonts: CodeFonts,
    style: &Style,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pass: &mut wgpu::RenderPass<'_>,
    screen: Vec2,
    caret_visible: bool,
) -> Frame {
    editor.doc.resolve_fonts(fonts, editor.italic_comments);
    let handle = text.shape(BlockKey(1), style, &editor.doc.keys(), &editor.doc);
    let Some(handle) = handle else {
        return Frame { handle: None };
    };
    let font_px = editor.font_px;
    let origin = Vec2::new(MARGIN, MARGIN - editor.scroll_y);
    let view_h = (screen.y - STATUS_H).max(0.);
    let view = Rect::new(0., 0., screen.x, view_h);
    let paint = cache.paint(text, &editor.doc, editor.alternate_palette);
    let placed = text.measure(handle).clamp_caret(editor.caret);
    let physical = editor.doc.rope.byte_to_line(placed.byte_index);
    let col = editor
        .doc
        .rope
        .byte_slice(editor.doc.rope.line_to_byte(physical)..placed.byte_index)
        .len_chars();
    let status = format!(
        "{}{} · C · Ln {}, Col {} · {:.0}px · F2 palette · F3 italic comments · Ctrl+S save",
        editor.doc.name(),
        if editor.doc.dirty { " •" } else { "" },
        physical + 1,
        col + 1,
        font_px
    );
    let chrome_style = Style {
        wrap_em: None,
        ..*style
    };
    let chrome = text.shape_transient(&status, &chrome_style);
    cache.body.prepare(
        text,
        device,
        queue,
        Draw {
            block: handle,
            at: origin,
            size: font_px,
            color: Color(FG),
            clip: Some(view),
            paint,
        },
    );
    if let Some(chrome) = chrome {
        cache.chrome.prepare(
            text,
            device,
            queue,
            Draw {
                block: chrome,
                at: Vec2::new(MARGIN, view_h + 5.),
                size: 12.,
                color: Color(STATUS_FG),
                ..Default::default()
            },
        );
    }
    // CPU clipping selects glyphs; the actual scissor also trims partially
    // visible glyphs and overlays at the viewport/status boundary.
    if view_h >= 1. {
        pass.set_scissor_rect(0, 0, screen.x as u32, view_h as u32);
        let layout = text.measure(handle);
        if let Some(range) = editor.selection() {
            for span in layout.selection(range) {
                rects.push(
                    origin.x + span.x_em * font_px,
                    origin.y + span.y_em * font_px,
                    (span.width_em * font_px).max(2.),
                    span.height_em * font_px,
                    SELECTION,
                    screen,
                );
            }
        }
        rects.flush(device, pass);
        cache.body.draw(text, pass);
        if caret_visible {
            let rect = layout.caret_rect_on_line(Some(placed.line_index), placed.byte_index);
            let height = rect.height_em.max(1.);
            let width = if editor.caret_block {
                layout
                    .line_range(placed.line_index)
                    .zip(layout.next_caret_stop(placed.byte_index))
                    .filter(|(range, next)| *next <= range.end)
                    .map(|(_, next)| {
                        (layout
                            .caret_rect_on_line(Some(placed.line_index), next)
                            .x_em
                            - rect.x_em)
                            .abs()
                    })
                    .filter(|w| *w > 0.05)
                    .unwrap_or(0.55)
                    * font_px
            } else {
                1.5
            };
            let mut color = CARET;
            if editor.caret_block {
                color[3] = 0.45;
            }
            rects.push(
                origin.x + rect.x_em * font_px,
                origin.y + rect.y_em * font_px,
                width,
                height * font_px,
                color,
                screen,
            );
            rects.flush(device, pass);
        }
    }
    pass.set_scissor_rect(0, 0, screen.x as u32, screen.y as u32);
    rects.push(0., view_h, screen.x, screen.y - view_h, STATUS_BG, screen);
    rects.flush(device, pass);
    if chrome.is_some() {
        cache.chrome.draw(text, pass);
    }
    Frame {
        handle: Some(handle),
    }
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
            WindowEvent::Resized(_) => gfx.window.request_redraw(),
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
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => {
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
    fonts: CodeFonts,
    cache: RenderCache,
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
                        .with_title("sanscale code-editor")
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
                label: Some("code-editor"),
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
        let fonts = fonts::load(&mut text, MONO_CHAIN, font);
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
            cache: RenderCache::default(),
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
        let wrap = (self.config.width as f32 - 2.0 * MARGIN) / self.editor.font_px;
        Style {
            chain: self.fonts.normal,
            wrap_em: Some(wrap.max(4.0)),
            align: Align::Left,
            line_spacing: 1.15,
        }
    }

    fn update_title(&self) {
        self.window.set_title(&format!(
            "{}{} — sanscale code-editor",
            self.editor.doc.name(),
            if self.editor.doc.dirty { " •" } else { "" },
        ));
    }

    /// Mouse → placed caret, through the same layout the last frame drew.
    /// `hit_test` answers with both the byte *and* the visual line, which is
    /// the affinity — a click near a soft break lands on the line you clicked.
    fn hit_at_cursor(&self) -> Option<Caret> {
        if self.cursor.y < 0. || self.cursor.y >= self.config.height as f32 - STATUS_H {
            return None;
        }
        let handle = self.last_handle?;
        let layout = self.text.measure(handle);
        let em = Vec2::new(
            (self.cursor.x - MARGIN) / self.editor.font_px,
            (self.cursor.y - MARGIN + self.editor.scroll_y) / self.editor.font_px,
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

    fn on_key(&mut self, event: winit::event::KeyEvent) {
        self.last_input = Instant::now();
        self.blink_phase = 0;
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

        let previous_revision = editor.doc.revision;
        let mut edited = true;
        match event.logical_key {
            Key::Named(NamedKey::F2) => {
                editor.alternate_palette = !editor.alternate_palette;
                edited = false;
            }
            Key::Named(NamedKey::F3) => {
                editor.italic_comments = !editor.italic_comments;
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
            self.editor
                .doc
                .resolve_fonts(self.fonts, self.editor.italic_comments);
            let keys = self.editor.doc.keys();
            let style = self.style();
            if let Some(handle) = self
                .text
                .shape(BlockKey(1), &style, &keys, &self.editor.doc)
            {
                self.last_handle = Some(handle);
                let view_h = self.config.height as f32;
                let layout = self.text.measure(handle);
                if self.editor.doc.revision != previous_revision {
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
            .add_filter("C source", &["c", "h"])
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

        let style = self.style();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("code-editor"),
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

// ---------------------------------------------------------------------------
// Headless dump: one composed frame, no window
// ---------------------------------------------------------------------------

const DUMP_TEXT: &str = r#"/* ring.c — real bold keywords, italic comments, one shared layout. */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

#define CAPACITY 8u

typedef struct {
    uint32_t values[CAPACITY];
    size_t read, count;
} Ring;

static bool push(Ring *ring, uint32_t value)
{
    if (ring->count == CAPACITY)
        return false;  // full: leave the queue unchanged

    size_t write = (ring->read + ring->count) % CAPACITY;
    ring->values[write] = value;
    ring->count += 1;
    return true;
}

int main(void)
{
    Ring queue = {0};
    push(&queue, 0x2Au);
    printf("ready: %zu items · 世界 🌍\n", queue.count);
    return 0;
}
"#;

fn dump_png(font: Option<&str>, path: Option<&std::path::Path>) {
    let harness = common::Harness::new(1060, 790);
    let mut text = TextService::new();
    let fonts = fonts::load(&mut text, MONO_CHAIN, font);
    let mut rects = RectPainter::new(&harness.device, harness.config.format);
    let content = path.map(|p| {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("open {}: {e}", p.display()))
    });
    let mut editor = Editor::new(Doc::from_text(
        content.as_deref().unwrap_or(DUMP_TEXT),
        Some(
            path.map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("ring.c")),
        ),
    ));
    let mut cache = RenderCache::default();
    if path.is_none() {
        let start = DUMP_TEXT.find("ring->read + ring->count").unwrap();
        let end = start + "ring->read + ring->count".len();
        editor.anchor = Some(start);
        editor.caret = Caret {
            byte_index: end,
            line_index: usize::MAX,
        };
    }
    let style = Style {
        chain: fonts.normal,
        wrap_em: Some((1060. - 2. * MARGIN) / editor.font_px),
        align: Align::Left,
        line_spacing: 1.15,
    };
    harness.save_png(&mut text, BG, "code-editor.png", |text, device, queue, pass| {
        render_frame(
            text,
            &mut rects,
            &mut cache,
            &mut editor,
            fonts,
            &style,
            device,
            queue,
            pass,
            Vec2::new(1060., 790.),
            true,
        );
    });
    println!("wrote code-editor.png");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service() -> (TextService, CodeFonts) {
        let mut text = TextService::new();
        let fonts = fonts::load(&mut text, &["DejaVu Sans Mono", "Consolas", "Menlo"], None);
        assert_ne!(
            fonts.normal, fonts.italic,
            "tests need a real italic/oblique face"
        );
        assert_ne!(fonts.normal, fonts.bold, "tests need a real bold face");
        (text, fonts)
    }
    fn style(fonts: CodeFonts) -> Style {
        Style {
            chain: fonts.normal,
            wrap_em: Some(20.),
            align: Align::Left,
            line_spacing: 1.15,
        }
    }
    #[test]
    fn ordinary_middle_edit_lexes_one_line_and_keeps_other_identities() {
        let (mut text, fonts) = service();
        let mut doc = Doc::from_text("int a=1;\nint b=2;\nint c=3;\n", None);
        doc.resolve_fonts(fonts, true);
        let before = doc.keys();
        let parsed = doc.lexed_lines;
        text.shape(BlockKey(1), &style(fonts), &before, &doc)
            .unwrap();
        let byte = doc.rope.to_string().find('2').unwrap();
        doc.replace(byte..byte + 1, "9");
        doc.resolve_fonts(fonts, true);
        let after = doc.keys();
        assert_eq!(doc.lexed_lines - parsed, 1);
        assert_eq!(before[0], after[0]);
        assert_ne!(before[1], after[1]);
        assert_eq!(before[2..], after[2..]);
        #[cfg(feature = "perf-counters")]
        sanscale::profiling::reset_work_counters();
        text.shape(BlockKey(1), &style(fonts), &after, &doc)
            .unwrap();
        #[cfg(feature = "perf-counters")]
        assert_eq!(sanscale::profiling::work_counters().shape_calls, 1);
        let parsed = doc.lexed_lines;
        let keys = doc.keys();
        doc.replace(byte..byte + 1, "9");
        assert_eq!(doc.lexed_lines, parsed);
        assert_eq!(doc.keys(), keys);
    }
    #[test]
    fn comment_state_propagates_only_until_it_matches_the_cached_suffix() {
        let (_, fonts) = service();
        let mut doc = Doc::from_text("int a;\nint b;\n*/ int c;\nint d;", None);
        doc.resolve_fonts(fonts, true);
        let before = doc.keys();
        let parsed = doc.lexed_lines;
        doc.replace(0..0, "/* ");
        doc.resolve_fonts(fonts, true);
        assert_eq!(doc.lexed_lines - parsed, 3);
        assert_eq!(doc.lines[1].tokens[0].kind, syntax::Kind::Comment);
        assert_ne!(doc.keys()[1], before[1]);
        assert_eq!(doc.keys()[3], before[3]);
        doc.replace(0..3, "");
        doc.resolve_fonts(fonts, true);
        assert_eq!(doc.lines[1].tokens[0].kind, syntax::Kind::Keyword);
    }
    #[test]
    fn palette_and_font_themes_do_not_lex_or_dirty_the_source() {
        let (mut text, fonts) = service();
        let mut doc = Doc::from_text("int n=1; // comment\nreturn n;", None);
        doc.resolve_fonts(fonts, true);
        let keys = doc.keys();
        let parsed = doc.lexed_lines;
        let mut cache = RenderCache::default();
        let a = cache.paint(&mut text, &doc, false).unwrap();
        let block = text.shape(BlockKey(1), &style(fonts), &keys, &doc).unwrap();
        #[cfg(feature = "perf-counters")]
        sanscale::profiling::reset_work_counters();
        assert_eq!(cache.paint(&mut text, &doc, false), Some(a));
        let b = cache.paint(&mut text, &doc, true).unwrap();
        assert_ne!(a, b);
        doc.resolve_fonts(fonts, true);
        assert_eq!(doc.keys(), keys);
        assert_eq!(
            text.shape(BlockKey(1), &style(fonts), &doc.keys(), &doc),
            Some(block)
        );
        #[cfg(feature = "perf-counters")]
        {
            let c = sanscale::profiling::work_counters();
            assert_eq!((c.shape_calls, c.flow_calls, c.source_reads), (0, 0, 0));
        }
        doc.resolve_fonts(fonts, false);
        let new = doc.keys();
        assert_ne!(new[0], keys[0]);
        assert_eq!(new[1], keys[1]);
        assert_eq!(cache.paint(&mut text, &doc, true), Some(b));
        assert_eq!(doc.lexed_lines, parsed);
        assert_eq!(doc.revision, 0);
        assert!(!doc.dirty);
    }
    #[test]
    fn split_merge_and_open_preserve_the_identity_contract() {
        let (mut text, fonts) = service();
        let mut doc = Doc::from_text("int a;\nint b;\nint c;", None);
        doc.resolve_fonts(fonts, true);
        let old = doc.keys();
        doc.replace(3..3, "\n");
        doc.resolve_fonts(fonts, true);
        assert_eq!(doc.keys()[2..], old[1..]);
        doc.replace(3..4, "");
        doc.resolve_fonts(fonts, true);
        assert_eq!(doc.keys()[1..], old[1..]);
        let first = text
            .shape(BlockKey(1), &style(fonts), &doc.keys(), &doc)
            .unwrap();
        let mut other = Doc::from_text("return 0;\nint x=7;\n// another file", None);
        other.resolve_fonts(fonts, true);
        assert_ne!(doc.namespace, other.namespace);
        let second = text
            .shape(BlockKey(1), &style(fonts), &other.keys(), &other)
            .unwrap();
        assert_eq!(first, second, "same block slot, new contents/revision");
        assert_eq!(text.measure(second).len_bytes(), other.rope.len_bytes());
    }
    #[test]
    fn styled_unicode_wrap_hit_testing_and_selection_share_byte_coordinates() {
        let (mut text, fonts) = service();
        // A combining mark after a lexical token end must stay in its grapheme.
        let mut doc = Doc::from_text("int\u{301} x = 12; // café\n/* note */\u{301} int y;", None);
        doc.resolve_fonts(fonts, true);
        let mut st = style(fonts);
        st.wrap_em = Some(8.);
        let h = text
            .shape(BlockKey(1), &st, &doc.keys(), &doc)
            .expect("grapheme-safe font spans");
        let layout = text.measure(h);
        assert!(layout.line_count() > doc.lines.len());
        assert_eq!(layout.len_bytes(), doc.rope.len_bytes());
        for line in 0..layout.line_count() {
            let range = layout.line_range(line).unwrap();
            let caret = layout.caret_rect_on_line(Some(line), range.start);
            let hit = layout
                .hit_test(Vec2::new(caret.x_em, caret.y_em + caret.height_em * 0.5))
                .unwrap();
            assert_eq!(hit.byte_index, range.start);
        }
        assert!(!layout.selection(0..doc.rope.len_bytes()).is_empty());
    }
    #[test]
    fn physical_lines_are_lf_only_and_paint_ranges_rebase_after_insertion() {
        let mut doc = Doc::from_text("int a;\n// b\u{2028} c", None);
        assert_eq!(doc.lines.len(), 2, "U+2028 in C text is not a physical LF");
        let spans = doc.paint_spans(false);
        let old = spans
            .iter()
            .find(|s| s.range.start == 7)
            .unwrap()
            .range
            .clone();
        doc.replace(0..0, "  ");
        let spans = doc.paint_spans(false);
        assert!(
            spans
                .iter()
                .any(|s| s.range == (old.start + 2..old.end + 2))
        );
    }
}
