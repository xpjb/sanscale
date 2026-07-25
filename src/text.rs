//! The redesigned public surface: one `Text` service, `Copy` handles into its pools.
//!
//! This module is the API skeleton from `decisions.md`, stood up with real types,
//! signatures and borrow semantics so a consumer migration can be compiled against
//! it. Bodies are stubs; the shape is the thing under test.
//!
//! Mental model: one service holding keyed pools with eviction. The consumer holds
//! `Copy` handles and consumer-minted keys; nothing borrows the service across a
//! call.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

/// Shared font bytes. Deliberately fontdb's `make_shared_face_data` return type, so
/// discovered bytes pass straight through with no copy and no re-wrap.
pub type FontData = Arc<dyn AsRef<[u8]> + Send + Sync>;

// ---------------------------------------------------------------------------
// value types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl From<(f32, f32)> for Vec2 {
    fn from((x, y): (f32, f32)) -> Self {
        Self { x, y }
    }
}

impl From<[f32; 2]> for Vec2 {
    fn from([x, y]: [f32; 2]) -> Self {
        Self { x, y }
    }
}

/// Min/size rectangle in the transform's source space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

impl From<[f32; 4]> for Rect {
    fn from([x, y, width, height]: [f32; 4]) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Linear RGBA. Draw-time only — never baked into shaping or geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl From<[f32; 4]> for Color {
    fn from(v: [f32; 4]) -> Self {
        Self(v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug)]
pub enum FontError {
    Parse,
    NoFaces,
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse => write!(f, "failed to parse font"),
            Self::NoFaces => write!(f, "font contains no usable faces"),
        }
    }
}

impl std::error::Error for FontError {}

// ---------------------------------------------------------------------------
// handles and keys
// ---------------------------------------------------------------------------

/// One mapped concrete font (pool 1). Deduped by data identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontHandle(u32);

/// An ordered fallback chain of fonts (pool 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontChainHandle(u16);

/// A shaped *block* — 1..N paragraphs flowed together into one coordinate space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShapedHandle(u32);

/// The consumer's identity for one paragraph: the unit of *invalidation*.
///
/// `namespace` keeps two documents' pool slots from colliding in the shared cache;
/// a collision here would render the wrong text, so it stays a separate field
/// rather than being hashed into `slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParagraphKey {
    pub namespace: u64,
    pub slot: u32,
    pub generation: u32,
}

/// The consumer's identity for a composed block: the unit of *coordinate space*.
///
/// Carries no version — change detection is comparing the parts, whose keys carry
/// their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey(pub u64);

/// Everything that affects shaping. Zero pixels, no color, so the cache is
/// zoom-invariant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub chain: FontChainHandle,
    /// Wrap width in em (`pane_px / font_px`), or `None` for no wrapping.
    pub wrap_em: Option<f32>,
    pub align: Align,
    /// Multiplier on the font's metric line height.
    pub line_spacing: f32,
}

impl Eq for Style {}

impl Hash for Style {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.chain.hash(state);
        self.wrap_em.map(f32::to_bits).hash(state);
        self.align.hash(state);
        self.line_spacing.to_bits().hash(state);
    }
}

// ---------------------------------------------------------------------------
// layout — em space, read-only, borrowed transiently from `measure`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct CaretHit {
    pub byte_index: usize,
    pub line_index: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct CaretRect {
    pub x_em: f32,
    pub y_em: f32,
    pub height_em: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct SelectionSpan {
    pub line: usize,
    pub x_em: f32,
    pub y_em: f32,
    pub width_em: f32,
    pub height_em: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct LineMetrics {
    pub top_em: f32,
    pub baseline_em: f32,
    pub height_em: f32,
    pub width_em: f32,
}

#[derive(Clone, Debug)]
struct CaretStop {
    byte_index: usize,
    x_em: f32,
}

#[derive(Clone, Debug)]
struct LayoutLine {
    byte_range: Range<usize>,
    metrics: LineMetrics,
    carets: Vec<CaretStop>,
}

/// Laid-out geometry for one block, in em space, with document-global byte
/// offsets across all of its paragraphs.
///
/// Borrow this from [`Text::measure`] at the point of use — do not store it. Every
/// query returns `Copy` or owned data precisely so nothing needs to outlive the
/// call, which is what keeps `&mut self` free for [`Text::draw`].
#[derive(Clone, Debug, Default)]
pub struct Layout {
    lines: Vec<LayoutLine>,
    width_em: f32,
    height_em: f32,
}

impl Layout {
    pub fn size_em(&self) -> Vec2 {
        Vec2::new(self.width_em, self.height_em)
    }

    pub fn width_em(&self) -> f32 {
        self.width_em
    }

    /// Total laid-out height — the scrollbar's content extent.
    pub fn height_em(&self) -> f32 {
        self.height_em
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, index: usize) -> Option<LineMetrics> {
        self.lines.get(index).map(|line| line.metrics)
    }

    pub fn line_range(&self, index: usize) -> Option<Range<usize>> {
        self.lines.get(index).map(|line| line.byte_range.clone())
    }

    pub fn hit_test(&self, at_em: Vec2) -> Option<CaretHit> {
        let _ = at_em;
        todo!("hit_test")
    }

    pub fn line_for_byte(&self, byte_index: usize) -> Option<usize> {
        let _ = byte_index;
        todo!("line_for_byte")
    }

    pub fn caret_on_line(&self, line_index: usize, x_em: f32) -> Option<usize> {
        let _ = (line_index, x_em);
        todo!("caret_on_line")
    }

    pub fn caret_position(&self, byte_index: usize) -> Vec2 {
        let _ = byte_index;
        todo!("caret_position")
    }

    pub fn caret_rect(&self, byte_index: usize) -> CaretRect {
        let _ = byte_index;
        todo!("caret_rect")
    }

    pub fn selection(&self, range: Range<usize>) -> Vec<SelectionSpan> {
        let _ = range;
        todo!("selection")
    }
}

// ---------------------------------------------------------------------------
// text source
// ---------------------------------------------------------------------------

/// Supplies a paragraph's text, by identity, on a shaping cache miss.
///
/// A trait rather than a closure for one concrete reason: the returned `Cow`
/// borrows from `&self`, so no lifetime has to be threaded through the caller's
/// own structures. A closure would force its text lifetime to be a parameter of
/// `shape` and of anything that stores it, which unifies with the caller's other
/// borrows. This is *not* the old provider trait — it takes `&self`, it is used
/// as `&dyn`, and it is never a generic parameter.
///
/// `index` is supplied alongside the key because `shape` calls this only for
/// parts that miss, so an implementor cannot assume it is called once per part in
/// order; one holding text positionally needs the index to find it.
pub trait ParagraphSource {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>>;
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

struct Font;
struct Chain;
struct Block;
struct ParaLayout;

/// One text service: every pool, every cache, and the GPU resources.
#[derive(Default)]
pub struct Text {
    fonts: Vec<Font>,
    chains: Vec<Chain>,
    blocks: Vec<Block>,

    block_lookup: HashMap<BlockKey, ShapedHandle>,
    paragraphs: HashMap<(ParagraphKey, Style), ParaLayout>,

    layout_scratch: Layout,
}

impl Text {
    /// Takes nothing: pipelines are per-target and lazy (see [`Text::set_target`]),
    /// and the atlas self-bounds to the device limit at first draw.
    pub fn new() -> Self {
        Self::default()
    }

    // -- fonts ---------------------------------------------------------------

    /// Map shared font bytes into the pool, deduping on data identity
    /// (`(ptr, len, face_index)`). Pass the *same* `Arc` for the same face across
    /// chains or the dedup — and the shared glyph cache it buys — won't fire.
    pub fn map_font(&mut self, data: FontData, face_index: u32) -> Result<FontHandle, FontError> {
        let _ = (data, face_index);
        todo!("map_font")
    }

    /// Register an ordered fallback chain. Stored as-is: `fonts[0]` is primary and
    /// defines line metrics.
    pub fn register_chain(&mut self, fonts: &[FontHandle]) -> FontChainHandle {
        let _ = fonts;
        todo!("register_chain")
    }

    /// Drop a chain and any shaping keyed to it. Fonts it referenced are freed once
    /// no chain holds them.
    pub fn drop_chain(&mut self, chain: FontChainHandle) {
        let _ = chain;
        todo!("drop_chain")
    }

    /// Drop every font, chain and cache. Atlas textures are retained.
    pub fn clear(&mut self) {
        let _ = &mut self.fonts;
        todo!("clear")
    }

    // -- shaping (GPU-free) --------------------------------------------------

    /// Shape a block: 1..N paragraphs flowed together, with document-global byte
    /// offsets. `fetch` runs only for parts that miss the cache; `None` from it
    /// means a stale identity and the whole block is skipped. Re-calling with an
    /// unchanged `parts` slice is a comparison, not a reflow.
    /// Shape a block: 1..N paragraphs flowed together, with document-global byte
    /// offsets. `source` is consulted only for parts that miss the cache; `None`
    /// from it means a stale identity and the whole block is skipped. Re-calling
    /// with an unchanged `parts` slice is a comparison, not a reflow.
    pub fn shape(
        &mut self,
        block: BlockKey,
        style: &Style,
        parts: &[ParagraphKey],
        source: &dyn ParagraphSource,
    ) -> Option<ShapedHandle> {
        let _ = (block, style, parts, source);
        todo!("shape")
    }

    /// Shape text that has no stable consumer identity — tooltips, transient
    /// labels, anything the consumer would otherwise have to invent a key for.
    /// Content-keyed internally (the paragraph pool keys on the text itself), so
    /// it still caches; it just can't survive an edit the way an identity can.
    pub fn shape_transient(&mut self, text: &str, style: &Style) -> Option<ShapedHandle> {
        let _ = (text, style);
        todo!("shape_transient")
    }

    /// One-paragraph convenience over [`Text::shape`] — titles, labels, fields.
    /// One-paragraph convenience over [`Text::shape`] — a title, a field.
    pub fn shape_one(
        &mut self,
        key: ParagraphKey,
        style: &Style,
        source: &dyn ParagraphSource,
    ) -> Option<ShapedHandle> {
        self.shape(BlockKey(key.namespace ^ u64::from(key.slot)), style, &[key], source)
    }

    /// Em-space geometry for a block. Borrow it for the length of the call; hold
    /// the [`ShapedHandle`], not this.
    pub fn measure(&self, h: ShapedHandle) -> &Layout {
        let _ = h;
        &self.layout_scratch
    }

    // -- drawing -------------------------------------------------------------

    /// Select the target format for the draws that follow; a pipeline is built and
    /// cached per format. Call once per pass.
    pub fn set_target(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        let _ = (device, format);
        todo!("set_target")
    }

    /// Set the transform applied to the local-em quads this service emits. Call
    /// once per pass. Screen-space is [`Text::pixel_ortho`]; world or 3D text is an
    /// MVP.
    pub fn set_transform(&mut self, queue: &wgpu::Queue, transform: [f32; 16]) {
        let _ = (queue, transform);
        todo!("set_transform")
    }

    /// Column-major ortho mapping `(0,0)..(width,height)` to clip space — the
    /// screen-space special case.
    pub fn pixel_ortho(width: u32, height: u32) -> [f32; 16] {
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        [
            2.0 / w,
            0.0,
            0.0,
            0.0,
            0.0,
            -2.0 / h,
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
            0.0,
            -1.0,
            1.0,
            0.0,
            1.0,
        ]
    }

    /// Record glyph quads for `h` into `pass`.
    ///
    /// `at` and `size` are in the transform's source space — screen pixels under
    /// [`Text::pixel_ortho`], world units under an MVP. `at` is the top-left of the
    /// block box (baseline is internal). `clip` culls lines and glyphs on the CPU
    /// and, under a pixel ortho, sets the scissor.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        h: ShapedHandle,
        at: Vec2,
        size: f32,
        color: Color,
        clip: Option<Rect>,
    ) {
        let _ = (device, queue, pass, h, at, size, color, clip);
        todo!("draw")
    }
}
