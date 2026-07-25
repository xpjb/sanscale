//! The public surface: one `Text` service, `Copy` handles into its pools.
//!
//! Mental model: one service holding keyed pools with eviction. The consumer
//! holds `Copy` handles and mints its own keys; nothing borrows the service
//! across a call, so `measure` and `draw` never fight over it.
//!
//! Two levels the consumer sees:
//! - a **paragraph** is the unit of *invalidation* — its key carries the
//!   consumer's version, and an edit reshapes only the paragraph it touched;
//! - a **block** is the unit of *coordinate space* — 1..N paragraphs flowed
//!   together into one byte range and one line list, which is what you measure,
//!   hit-test and draw.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use crate::cache::{GlyphCache, GlyphInfo};
use crate::emoji::{bucket_for, EmojiCache};
use crate::flow::{flow_paragraph, FlowLine};
use crate::font::Font;
use crate::layout::{shape_text, ChainFont, ShapedGlyph};
use crate::renderer::{EmojiAtlas, EmojiRenderer, TextAtlas, TextRenderer};
use crate::vertex::{push_emoji_quad, push_glyph_quad_pixels, EmojiVertex, TextVertex};

/// Shared font bytes. Deliberately fontdb's `make_shared_face_data` return type,
/// so bytes a consumer already discovered pass straight through — no copy, no
/// re-wrap. Also takes `Arc::new(vec)` or `Arc::new(include_bytes!(..))`.
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

    fn max_x(&self) -> f32 {
        self.x + self.width
    }

    fn max_y(&self) -> f32 {
        self.y + self.height
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

/// Linear RGBA. A draw parameter only — never baked into shaping, so the same
/// shaped block redraws in any color for free.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl From<[f32; 4]> for Color {
    fn from(v: [f32; 4]) -> Self {
        Self(v)
    }
}

/// Horizontal alignment of each line within the wrap width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontError {
    /// The bytes are not a font this build can parse.
    Parse,
    /// The font pool is full (65 535 faces).
    PoolFull,
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse => write!(f, "failed to parse font"),
            Self::PoolFull => write!(f, "font pool is full"),
        }
    }
}

impl std::error::Error for FontError {}

// ---------------------------------------------------------------------------
// handles and keys
// ---------------------------------------------------------------------------

/// One mapped concrete font. Deduped by data identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontHandle(u16);

/// An ordered fallback chain of fonts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontChainHandle(u16);

/// A shaped *block* — 1..N paragraphs flowed into one coordinate space.
///
/// Carries a generation alongside its slot. Eviction frees slots and later
/// `shape` calls reuse them, so without this a handle the consumer cached would
/// silently start pointing at a different block — the glyphs would still draw,
/// just the wrong ones. A stale handle now measures empty and draws nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShapedHandle {
    slot: u32,
    generation: u32,
}

/// The consumer's identity for one paragraph: the unit of *invalidation*.
///
/// `namespace` keeps two documents' pool slots from colliding in the shared
/// cache. It stays a separate field rather than being hashed into `slot` because
/// a collision here renders the *wrong text*, silently and persistently.
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

/// Everything that affects shaping. Zero pixels and no color, so the cache is
/// zoom-invariant: moving the camera re-runs nothing.
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

impl Style {
    fn max_width_em(&self) -> f32 {
        self.wrap_em.unwrap_or(f32::MAX).max(0.0)
    }
}

// ---------------------------------------------------------------------------
// layout — em space, read-only, borrowed transiently from `measure`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, Default)]
pub struct LineMetrics {
    pub top_em: f32,
    pub baseline_em: f32,
    pub height_em: f32,
    pub width_em: f32,
}

/// One caret position on a line: a byte offset and where it sits, in em.
#[derive(Clone, Copy, Debug)]
pub struct CaretStop {
    pub byte_index: usize,
    pub x_em: f32,
}

/// One line's worth of synthetic layout, for [`Layout::from_lines`].
#[derive(Clone, Debug)]
pub struct LayoutLineSpec {
    pub byte_range: Range<usize>,
    pub metrics: LineMetrics,
    pub carets: Vec<CaretStop>,
}

#[derive(Clone, Debug)]
struct LayoutLine {
    byte_range: Range<usize>,
    metrics: LineMetrics,
    carets: Vec<CaretStop>,
    /// Alignment offset within the block width, em.
    align_em: f32,
    /// Glyphs with `x` relative to the line origin. Empty for a synthetic layout.
    glyphs: Vec<ShapedGlyph>,
}

/// Laid-out geometry for one block, in em space, with block-global byte offsets
/// across all of its paragraphs.
///
/// Borrow this from [`Text::measure`] **at the point of use** — do not store it.
/// Every query returns `Copy` or owned data precisely so nothing needs to outlive
/// the call, which is what keeps `&mut self` free for [`Text::draw`]. Pass the
/// [`ShapedHandle`] around instead; that is what it is for.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    lines: Vec<LayoutLine>,
    width_em: f32,
    height_em: f32,
}

impl Layout {
    /// Build a layout directly from line geometry, with no font and no shaping.
    ///
    /// Caret motion, selection and hit-testing are pure geometry, so a consumer
    /// should be able to unit-test its editor against a synthetic layout without
    /// loading a font or touching a GPU.
    pub fn from_lines(lines: Vec<LayoutLineSpec>) -> Self {
        let width_em = lines
            .iter()
            .map(|line| line.metrics.width_em)
            .fold(0.0f32, f32::max);
        let height_em = lines
            .last()
            .map(|line| line.metrics.top_em + line.metrics.height_em)
            .unwrap_or(0.0);
        Self {
            lines: lines
                .into_iter()
                .map(|line| LayoutLine {
                    byte_range: line.byte_range,
                    metrics: line.metrics,
                    carets: line.carets,
                    align_em: 0.0,
                    glyphs: Vec::new(),
                })
                .collect(),
            width_em,
            height_em,
        }
    }

    /// Widest line, em.
    pub fn width_em(&self) -> f32 {
        self.width_em
    }

    /// Total laid-out height, em — the scrollbar's content extent.
    pub fn height_em(&self) -> f32 {
        self.height_em
    }

    pub fn size_em(&self) -> Vec2 {
        Vec2::new(self.width_em, self.height_em)
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

    /// Total byte length of the block, including newlines between paragraphs.
    pub fn len_bytes(&self) -> usize {
        self.lines
            .last()
            .map(|line| line.byte_range.end)
            .unwrap_or(0)
    }

    /// The line containing `y_em`, clamped to the first/last so a drag past
    /// either end still resolves.
    fn line_index_for_y(&self, y_em: f32) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        for (index, line) in self.lines.iter().enumerate() {
            if y_em < line.metrics.top_em + line.metrics.height_em {
                return Some(index);
            }
        }
        Some(self.lines.len() - 1)
    }

    /// Caret nearest a point, in em relative to the block's top-left.
    pub fn hit_test(&self, at_em: Vec2) -> Option<CaretHit> {
        let line_index = self.line_index_for_y(at_em.y)?;
        let byte_index = self.caret_on_line(line_index, at_em.x)?;
        Some(CaretHit {
            byte_index,
            line_index,
        })
    }

    /// The line a byte offset falls on. A byte at a soft break belongs to the
    /// line that *starts* with it, which is what word-wrap caret affinity needs.
    pub fn line_for_byte(&self, byte_index: usize) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        for (index, line) in self.lines.iter().enumerate() {
            if byte_index < line.byte_range.end {
                return Some(index);
            }
        }
        Some(self.lines.len() - 1)
    }

    /// Nearest caret stop on `line_index` to `x_em`.
    pub fn caret_on_line(&self, line_index: usize, x_em: f32) -> Option<usize> {
        let line = self.lines.get(line_index)?;
        let local_x = x_em - line.align_em;
        line.carets
            .iter()
            .min_by(|a, b| (a.x_em - local_x).abs().total_cmp(&(b.x_em - local_x).abs()))
            .map(|caret| caret.byte_index)
    }

    /// Top-left of the caret for `byte_index`, em.
    pub fn caret_position(&self, byte_index: usize) -> Vec2 {
        let rect = self.caret_rect(byte_index);
        Vec2::new(rect.x_em, rect.y_em)
    }

    pub fn caret_rect(&self, byte_index: usize) -> CaretRect {
        self.caret_rect_on_line(self.line_for_byte(byte_index), byte_index)
    }

    /// Caret rect resolved on a specific line — the affinity hint an editor
    /// carries so a caret at a soft break stays where the user put it.
    pub fn caret_rect_on_line(&self, line_index: Option<usize>, byte_index: usize) -> CaretRect {
        let empty = CaretRect {
            x_em: 0.0,
            y_em: 0.0,
            height_em: 0.0,
        };
        let Some(index) = line_index.or_else(|| self.line_for_byte(byte_index)) else {
            return empty;
        };
        let Some(line) = self.lines.get(index) else {
            return empty;
        };
        CaretRect {
            x_em: caret_x_on(line, byte_index) + line.align_em,
            y_em: line.metrics.top_em,
            height_em: line.metrics.height_em,
        }
    }

    /// Highlight rects for a byte range, one per covered line.
    pub fn selection(&self, range: Range<usize>) -> Vec<SelectionSpan> {
        if range.is_empty() {
            return Vec::new();
        }
        let mut spans = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            let start = range.start.max(line.byte_range.start);
            let end = range.end.min(line.byte_range.end);
            if start >= end {
                continue;
            }
            let x0 = caret_x_on(line, start);
            let x1 = caret_x_on(line, end);
            spans.push(SelectionSpan {
                line: index,
                x_em: x0.min(x1) + line.align_em,
                y_em: line.metrics.top_em,
                width_em: (x1 - x0).abs(),
                height_em: line.metrics.height_em,
            });
        }
        spans
    }
}

fn caret_x_on(line: &LayoutLine, byte_index: usize) -> f32 {
    line.carets
        .iter()
        .find(|caret| caret.byte_index == byte_index)
        .map(|caret| caret.x_em)
        .unwrap_or(if byte_index <= line.byte_range.start {
            0.0
        } else {
            line.metrics.width_em
        })
}

// ---------------------------------------------------------------------------
// text source
// ---------------------------------------------------------------------------

/// Supplies a paragraph's text, by identity, on a shaping cache miss.
///
/// A trait rather than a closure for one concrete reason: the returned `Cow`
/// borrows from `&self`, so no lifetime has to be threaded through the caller's
/// own structures. A closure would force its text lifetime to be a parameter of
/// [`Text::shape`] and of anything storing it, which then unifies with the
/// caller's other borrows.
///
/// This is not a heavyweight provider: `&self`, used as `&dyn`, never a generic.
///
/// `index` comes alongside the key because `shape` calls this only for parts that
/// *miss* — an implementor cannot assume one in-order call per part, so one
/// holding its text positionally needs the index to find it.
pub trait ParagraphSource {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>>;
}

/// A source over already-materialized paragraphs, for consumers holding strings.
pub struct Paragraphs<'a>(pub &'a [&'a str]);

impl ParagraphSource for Paragraphs<'_> {
    fn paragraph_text(&self, index: usize, _key: ParagraphKey) -> Option<Cow<'_, str>> {
        self.0.get(index).map(|text| Cow::Borrowed(*text))
    }
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// Level-2 result for one paragraph at one style.
struct ParaLayout {
    lines: Vec<FlowLine>,
    len_bytes: usize,
    last_used: u64,
}

struct BlockSlot {
    block: Option<Block>,
}

struct Block {
    key: BlockKey,
    style: Style,
    parts: Vec<ParagraphKey>,
    layout: Layout,
    last_used: u64,
}

/// What the cached vertices were built for. Color and size are baked into the
/// vertex format today, so they are part of the key; moving them to a per-draw
/// uniform would reduce this to the clip alone.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GeomKey {
    size: u32,
    /// Quads are position-baked, so placement is part of the key. Pan and zoom
    /// move the *transform*, not `at`, so a scrolling canvas still hits.
    at: [u32; 2],
    color: [u32; 4],
    clip: Option<[u32; 4]>,
}

/// A block's quads, kept on the **CPU**.
///
/// Deliberately not a GPU buffer per block: quads live in the transform's source
/// space, so they survive pan and zoom, and keeping them host-side lets many
/// blocks concatenate into one upload and one draw call. A buffer per block
/// means a draw call per block, which is ruinous on a dense canvas.
struct Geometry {
    key: GeomKey,
    /// Emoji atlas eviction epoch the UVs were baked against. A change means a
    /// cell was recycled and the cached quads may now point at another glyph —
    /// the invalidation the old public `emoji_epoch` made the *consumer* do.
    epoch: u64,
    text: Vec<TextVertex>,
    emoji: Vec<EmojiVertex>,
}

/// One block to draw, for [`Text::draw_batch`].
#[derive(Clone, Copy, Debug)]
pub struct Draw {
    pub block: ShapedHandle,
    /// Top-left of the block box, in the transform's source space.
    pub at: Vec2,
    pub size: f32,
    pub color: Color,
    pub clip: Option<Rect>,
}

struct Gpu {
    format: wgpu::TextureFormat,
    text: TextRenderer,
    emoji: EmojiRenderer,
    text_atlas: TextAtlas,
    emoji_atlas: EmojiAtlas,
}

/// Cache bounds. Eviction is capacity-based rather than time-based on purpose:
/// the consumer holds `ShapedHandle`s for as long as it likes, and the service
/// cannot know a handle is dead. A time-based TTL would evict blocks a consumer
/// still holds — which the generation check makes *safe* (it draws nothing) but
/// not *correct*. Under a capacity bound, normal workloads never evict at all,
/// and a pathological one loses its least-recently-used blocks rather than its
/// oldest-shaped ones.
const MAX_BLOCKS: usize = 1 << 17;
const MAX_PARAGRAPHS: usize = 1 << 17;

/// One text service: every pool, every cache, and the GPU resources.
///
/// Lives *beside* the consumer's renderer, never inside it: nothing here needs a
/// device except [`Text::draw`], which takes one as a parameter. That is what
/// lets model code hold a `&Text` for measurement and hit-testing without
/// reaching for the GPU, and what makes shaping work headless.
#[derive(Default)]
pub struct Text {
    fonts: Vec<Font>,
    /// `None` for a dropped slot, so live handles stay valid across a reload.
    chains: Vec<Option<Vec<FontHandle>>>,

    blocks: Vec<BlockSlot>,
    /// Slot generations, held apart from `blocks` so validating a handle touches
    /// a dense 4-byte array instead of pulling a ~200-byte `BlockSlot` into cache.
    generations: Vec<u32>,
    /// Pool 7, also held apart from `blocks`. `draw_batch` walks this and nothing
    /// else: on a dense frame it is the only array in the hot loop, and keeping it
    /// off `Block` is the difference between touching ~96 bytes per item and
    /// chasing through the block's layout, parts and style to reach it.
    geometry: Vec<Option<Geometry>>,
    /// Tombstoned block slots, newest first. Allocation pops from here instead of
    /// scanning `blocks` for a hole — a linear scan per `shape` is quadratic over
    /// a frame that shapes thousands of small blocks. Slots are never moved, so
    /// live handles stay valid.
    free_blocks: Vec<u32>,
    block_lookup: HashMap<BlockKey, ShapedHandle>,
    paragraphs: HashMap<(ParagraphKey, Style), ParaLayout>,

    glyphs: GlyphCache,
    emoji: EmojiCache,
    gpu: Option<Gpu>,
    pending_format: Option<wgpu::TextureFormat>,

    clock: u64,
    empty_layout: Layout,
    batch_text: Vec<TextVertex>,
    batch_emoji: Vec<EmojiVertex>,
}

impl Text {
    /// Takes nothing: pipelines are per-target and lazy (see [`Text::set_target`]),
    /// and the atlases size themselves.
    pub fn new() -> Self {
        Self::default()
    }

    // -- fonts ---------------------------------------------------------------

    /// Map shared font bytes into the pool, deduping on data identity.
    ///
    /// Pass the *same* `Arc` for the same face across chains, or the dedup — and
    /// the shared glyph cache it buys — won't fire. With fontdb that means one
    /// long-lived `Database` and `make_shared_face_data`, not a fresh read per
    /// chain.
    pub fn map_font(&mut self, data: FontData, face_index: u32) -> Result<FontHandle, FontError> {
        let font = Font::from_shared(data, face_index)?;
        let identity = font.data_identity();
        if let Some(existing) = self
            .fonts
            .iter()
            .position(|candidate| candidate.data_identity() == identity)
        {
            return Ok(FontHandle(existing as u16));
        }
        if self.fonts.len() >= u16::MAX as usize {
            return Err(FontError::PoolFull);
        }
        self.fonts.push(font);
        Ok(FontHandle(self.fonts.len() as u16 - 1))
    }

    /// Register an ordered fallback chain. Stored as-is: `fonts[0]` is primary and
    /// defines the line metrics for anything shaped with it.
    pub fn register_chain(&mut self, fonts: &[FontHandle]) -> FontChainHandle {
        let slot = self
            .chains
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.chains.push(None);
                self.chains.len() - 1
            });
        self.chains[slot] = Some(fonts.to_vec());
        FontChainHandle(slot as u16)
    }

    /// Drop a chain and every block and paragraph shaped with it.
    ///
    /// Fonts stay in the pool — another chain may share them — but nothing keeps
    /// the old chain's layouts alive. This is what a settings font change needs,
    /// and what the old `Box::leak`ed font bytes made impossible.
    pub fn drop_chain(&mut self, chain: FontChainHandle) {
        let Some(slot) = self.chains.get_mut(chain.0 as usize) else {
            return;
        };
        *slot = None;
        self.paragraphs.retain(|(_, style), _| style.chain != chain);
        for (index, slot) in self.blocks.iter_mut().enumerate() {
            let stale = slot
                .block
                .as_ref()
                .is_some_and(|block| block.style.chain == chain);
            if stale {
                if let Some(block) = slot.block.take() {
                    self.block_lookup.remove(&block.key);
                    self.free_blocks.push(index as u32);
                }
            }
        }
    }

    /// Drop every font, chain and cached layout. Atlas textures are retained —
    /// they are sized, not populated, state.
    pub fn clear(&mut self) {
        self.fonts.clear();
        self.chains.clear();
        self.blocks.clear();
        self.generations.clear();
        self.geometry.clear();
        self.free_blocks.clear();
        self.block_lookup.clear();
        self.paragraphs.clear();
        self.glyphs = GlyphCache::new();
        self.emoji = EmojiCache::new();
    }

    // -- shaping (GPU-free) --------------------------------------------------

    /// Shape a block: 1..N paragraphs flowed together, with block-global byte
    /// offsets (paragraphs joined by a newline, exactly as the consumer's
    /// document reads).
    ///
    /// `source` is consulted only for parts that miss the cache; `None` from it
    /// means a stale identity and the whole block is skipped. Re-calling with an
    /// unchanged `parts` slice at the same style is a comparison, not a reflow —
    /// so an unedited paragraph is never reshaped, and a camera move never
    /// reshapes anything at all.
    pub fn shape(
        &mut self,
        block: BlockKey,
        style: &Style,
        parts: &[ParagraphKey],
        source: &dyn ParagraphSource,
    ) -> Option<ShapedHandle> {
        self.clock += 1;
        if parts.is_empty() || self.chain_fonts(style.chain).is_none() {
            return None;
        }

        // Unchanged parts at the same style: the block is already correct, so
        // this is a comparison rather than a reflow.
        if let Some(&handle) = self.block_lookup.get(&block) {
            if let Some(index) = self.block_index(handle) {
                if let Some(existing) = self.blocks[index].block.as_mut() {
                    if existing.style == *style && existing.parts == parts {
                        existing.last_used = self.clock;
                        return Some(handle);
                    }
                }
            }
        }

        for (index, key) in parts.iter().enumerate() {
            self.ensure_paragraph(*key, style, index, source)?;
        }
        let layout = self.assemble(style, parts);

        // Reuse this block's own slot if it still holds one; otherwise take a
        // free slot and bump its generation so any handle to the old occupant
        // stops resolving.
        let index = match self.block_lookup.get(&block) {
            Some(handle) if (handle.slot as usize) < self.blocks.len() => handle.slot as usize,
            _ => match self.free_blocks.pop() {
                Some(slot) => slot as usize,
                None => {
                    self.blocks.push(BlockSlot { block: None });
                    self.generations.push(0);
                    self.geometry.push(None);
                    self.blocks.len() - 1
                }
            },
        };
        if self.blocks[index].block.is_none() {
            self.generations[index] = self.generations[index].wrapping_add(1);
        }
        self.blocks[index].block = Some(Block {
            key: block,
            style: *style,
            parts: parts.to_vec(),
            layout,
            last_used: self.clock,
        });
        self.geometry[index] = None;
        let handle = ShapedHandle {
            slot: index as u32,
            generation: self.generations[index],
        };
        self.block_lookup.insert(block, handle);
        // Only sweeps when a pool is actually over its bound, and the check
        // itself is O(1) — a frame that shapes thousands of blocks stays linear.
        self.evict();
        Some(handle)
    }

    /// One-paragraph convenience over [`Text::shape`] — a title, a field.
    pub fn shape_one(
        &mut self,
        key: ParagraphKey,
        style: &Style,
        source: &dyn ParagraphSource,
    ) -> Option<ShapedHandle> {
        let block = BlockKey(
            key.namespace.rotate_left(17) ^ (u64::from(key.slot) << 32 | u64::from(key.generation)),
        );
        self.shape(block, style, &[key], source)
    }

    /// Shape text with no stable consumer identity — a tooltip, a transient
    /// label, anything the consumer would otherwise have to invent a key for.
    ///
    /// Content-keyed, so it still caches; it just cannot survive an edit the way
    /// an identity can. Newlines split it into paragraphs, as everywhere else.
    pub fn shape_transient(&mut self, text: &str, style: &Style) -> Option<ShapedHandle> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let content = hasher.finish();
        let lines: Vec<&str> = text.split('\n').collect();
        let parts: Vec<ParagraphKey> = (0..lines.len())
            .map(|index| ParagraphKey {
                // A reserved namespace, so a content key can never alias one of
                // the consumer's document identities.
                namespace: u64::MAX,
                slot: (content as u32) ^ (index as u32),
                generation: (content >> 32) as u32,
            })
            .collect();
        self.shape(BlockKey(content), style, &parts, &Paragraphs(&lines))
    }

    /// Em-space geometry for a block. Borrow it for the length of the call and
    /// hold the [`ShapedHandle`], not this.
    pub fn measure(&self, h: ShapedHandle) -> &Layout {
        self.block(h)
            .map(|block| &block.layout)
            .unwrap_or(&self.empty_layout)
    }

    /// Resolve a handle, rejecting one whose slot has since been reused.
    fn block(&self, h: ShapedHandle) -> Option<&Block> {
        let index = self.block_index(h)?;
        self.blocks[index].block.as_ref()
    }

    /// Validate a handle. Touches only the dense generation array — the block
    /// itself is never pulled into cache on the batch fast path.
    fn block_index(&self, h: ShapedHandle) -> Option<usize> {
        let index = h.slot as usize;
        (self.generations.get(index) == Some(&h.generation)).then_some(index)
    }

    /// Coverage and cache introspection.
    pub fn diagnostics(&self) -> Diagnostics<'_> {
        Diagnostics { text: self }
    }

    // -- drawing -------------------------------------------------------------

    /// Select the target format for the draws that follow; a pipeline is built
    /// and cached per format. Call once per pass.
    pub fn set_target(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        self.pending_format = Some(format);
        self.ensure_gpu(device);
    }

    /// Set the transform applied to the local-em quads this service emits. Call
    /// once per pass. Screen-space is [`Text::pixel_ortho`]; world or 3D text is
    /// an MVP — same call, no mode flag.
    pub fn set_transform(&mut self, queue: &wgpu::Queue, transform: [f32; 16]) {
        if let Some(gpu) = &self.gpu {
            let matrix = glam::Mat4::from_cols_array(&transform);
            gpu.text.write_matrix(queue, matrix);
            gpu.emoji.write_matrix(queue, matrix);
        }
    }

    /// Column-major ortho mapping `(0,0)..(width,height)` to clip space, y down —
    /// the screen-space special case.
    pub fn pixel_ortho(width: u32, height: u32) -> [f32; 16] {
        glam::Mat4::orthographic_rh(
            0.0,
            width.max(1) as f32,
            height.max(1) as f32,
            0.0,
            -1.0,
            1.0,
        )
        .to_cols_array()
    }

    /// Record glyph quads for `h` into `pass`.
    ///
    /// `at` and `size` are in the transform's source space — screen pixels under
    /// [`Text::pixel_ortho`], world units under an MVP. `at` is the **top-left**
    /// of the block box; the baseline is internal, so hit-testing and rendering
    /// cannot disagree about it. `clip` culls lines and glyphs on the CPU before
    /// anything is emitted, which is what keeps a scrolled long document cheap.
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
        self.draw_batch(
            device,
            queue,
            pass,
            &[Draw { block: h, at, size, color, clip }],
        );
    }

    /// Record many blocks in **one** pair of draw calls.
    ///
    /// Each block's quads are cached and reused across frames while its draw
    /// parameters hold, then concatenated into a single upload. This is what a
    /// dense canvas needs: drawing tens of thousands of glyph-sized blocks one
    /// call each costs hundreds of milliseconds in command recording alone, and
    /// none of that work is about text.
    pub fn draw_batch(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        items: &[Draw],
    ) {
        self.ensure_gpu(device);
        if self.gpu.is_none() || items.is_empty() {
            return;
        }

        let epoch = self.emoji.epoch();
        let mut text_verts = std::mem::take(&mut self.batch_text);
        let mut emoji_verts = std::mem::take(&mut self.batch_emoji);
        text_verts.clear();
        emoji_verts.clear();
        // One growth up front. A dense frame concatenates tens of thousands of
        // small runs, and letting the Vec double its way there is most of the
        // batch cost.
        text_verts.reserve(items.len() * 6);

        for item in items {
            let Some(index) = self.block_index(item.block) else {
                continue;
            };
            let key = GeomKey {
                size: item.size.to_bits(),
                at: [item.at.x, item.at.y].map(f32::to_bits),
                color: item.color.0.map(f32::to_bits),
                clip: item.clip.map(|c| [c.x, c.y, c.width, c.height].map(f32::to_bits)),
            };
            // Fast path: one dense-array probe, then straight into the batch.
            if let Some(geom) = self.geometry[index].as_ref() {
                // The epoch only matters if this block actually baked emoji UVs;
                // an atlas eviction cannot disturb a text-only block, and
                // invalidating those too rebuilds the whole frame for nothing.
                if geom.key == key && (geom.emoji.is_empty() || geom.epoch == epoch) {
                    text_verts.extend_from_slice(&geom.text);
                    if !geom.emoji.is_empty() {
                        emoji_verts.extend_from_slice(&geom.emoji);
                    }
                    continue;
                }
            }
            self.rebuild_geometry(index, item, key, epoch);
            if let Some(geom) = self.geometry[index].as_ref() {
                text_verts.extend_from_slice(&geom.text);
                if !geom.emoji.is_empty() {
                    emoji_verts.extend_from_slice(&geom.emoji);
                }
            }
        }

        // Atlas uploads are `queue` writes, which land before this command buffer
        // executes, so *adding* glyphs mid-pass is safe. The invariants that keep
        // it safe — never recreate a texture already bound by an earlier draw in
        // this pass, never evict a slot that draw sampled — live with the atlases.
        let gpu = self.gpu.as_mut().expect("checked above");
        gpu.text_atlas
            .sync(device, queue, &gpu.text.atlas_layout, &self.glyphs);
        gpu.emoji_atlas
            .sync(device, queue, &gpu.emoji.atlas_layout, &self.emoji);

        if !text_verts.is_empty() {
            let buffer = TextRenderer::build_vertices(device, &text_verts);
            gpu.text
                .draw_vertices(pass, &gpu.text_atlas, &buffer, 0..text_verts.len() as u32);
        }
        if !emoji_verts.is_empty() {
            let buffer = EmojiRenderer::build_vertices(device, &emoji_verts);
            gpu.emoji
                .draw(pass, &gpu.emoji_atlas, &buffer, 0..emoji_verts.len() as u32);
        }

        self.batch_text = text_verts;
        self.batch_emoji = emoji_verts;
    }

    /// Build one block's quads into its geometry cache. CPU only — no device and
    /// no upload; `draw_batch` does that once for the whole batch.
    fn rebuild_geometry(&mut self, index: usize, item: &Draw, key: GeomKey, epoch: u64) {
        let (at, size, color, clip) = (item.at, item.size, item.color, item.clip);
        let mut text_verts = Vec::new();
        let mut emoji_verts = Vec::new();

        // Emoji rasterization mutates the emoji cache, so it cannot happen while
        // the block is borrowed; collect the requests, then service them.
        let mut emoji_requests: Vec<(u16, u32, f32, f32)> = Vec::new();
        {
            let block = self.blocks[index].block.as_ref().expect("checked by caller");
            for line in &block.layout.lines {
                let top = at.y + line.metrics.top_em * size;
                let bottom = top + line.metrics.height_em * size;
                if clip.is_some_and(|clip| bottom < clip.y || top > clip.max_y()) {
                    continue;
                }
                let baseline = at.y + line.metrics.baseline_em * size;
                let origin_x = at.x + line.align_em * size;
                for glyph in &line.glyphs {
                    let pen_x = origin_x + glyph.x * size;
                    let pen_y = baseline - glyph.y * size;
                    if glyph.is_color {
                        let outside = clip.is_some_and(|clip| {
                            pen_y < clip.y
                                || pen_y - size > clip.max_y()
                                || pen_x + size < clip.x
                                || pen_x > clip.max_x()
                        });
                        if !outside {
                            emoji_requests.push((glyph.font_id, glyph.glyph_id, pen_x, pen_y));
                        }
                        continue;
                    }
                    let Some(info) = glyph.info else { continue };
                    if clip.is_some_and(|clip| !glyph_intersects(&info, pen_x, pen_y, size, clip)) {
                        continue;
                    }
                    push_glyph_quad_pixels(&mut text_verts, &info, pen_x, pen_y, size, color.0);
                }
            }
        }

        let bucket = bucket_for(size);
        for (font_id, glyph_id, pen_x, pen_y) in emoji_requests {
            let Some(font) = self.fonts.get(font_id as usize) else {
                continue;
            };
            if let Some(slot) = self
                .emoji
                .get_or_insert(font.face(), font_id, glyph_id, bucket)
            {
                let uv_min = [slot.x as f32, slot.y as f32];
                let uv_max = [(slot.x + slot.size) as f32, (slot.y + slot.size) as f32];
                push_emoji_quad(&mut emoji_verts, pen_x, pen_y, size, uv_min, uv_max);
            }
        }

        self.geometry[index] = Some(Geometry {
            key,
            epoch,
            text: text_verts,
            emoji: emoji_verts,
        });
    }

    // -- internals -----------------------------------------------------------

    fn ensure_gpu(&mut self, device: &wgpu::Device) {
        let Some(format) = self.pending_format else {
            return;
        };
        if self.gpu.as_ref().is_some_and(|gpu| gpu.format == format) {
            return;
        }
        let text = TextRenderer::for_format(device, format);
        let emoji = EmojiRenderer::for_format(device, format);
        // Bound the emoji atlas to what this device can hold, so it never grows
        // into a silent failure.
        self.emoji
            .set_max_height(device.limits().max_texture_dimension_2d);
        let text_atlas = TextAtlas::empty(device, &text.atlas_layout);
        let emoji_atlas = EmojiAtlas::empty(device, &emoji.atlas_layout, &self.emoji);
        self.gpu = Some(Gpu {
            format,
            text,
            emoji,
            text_atlas,
            emoji_atlas,
        });
    }

    fn chain_fonts(&self, chain: FontChainHandle) -> Option<&[FontHandle]> {
        self.chains
            .get(chain.0 as usize)
            .and_then(Option::as_ref)
            .map(Vec::as_slice)
            .filter(|fonts| !fonts.is_empty())
    }

    fn chain_view(&self, chain: FontChainHandle) -> Vec<ChainFont<'_>> {
        chain_view(&self.fonts, &self.chains, chain)
    }

    /// Shape and flow one paragraph if it isn't already cached at this style.
    fn ensure_paragraph(
        &mut self,
        key: ParagraphKey,
        style: &Style,
        index: usize,
        source: &dyn ParagraphSource,
    ) -> Option<()> {
        let clock = self.clock;
        if let Some(entry) = self.paragraphs.get_mut(&(key, *style)) {
            entry.last_used = self.clock;
            return Some(());
        }
        // The only place text is ever pulled from the consumer.
        let text = source.paragraph_text(index, key)?;
        // Split the borrow: the chain view reads `fonts`, shaping writes
        // `glyphs`. They are disjoint fields, but a `&self` helper would tie them
        // together.
        let Self {
            fonts,
            chains,
            glyphs,
            paragraphs,
            ..
        } = self;
        let chain = chain_view(fonts, chains, style.chain);
        if chain.is_empty() {
            return None;
        }
        let run = shape_text(&chain, glyphs, text.as_ref());
        let lines = flow_paragraph(text.as_ref(), &run.glyphs, style.max_width_em());
        paragraphs.insert(
            (key, *style),
            ParaLayout {
                lines,
                len_bytes: text.len(),
                last_used: clock,
            },
        );
        Some(())
    }

    /// Concatenate the parts' cached line boxes into one block-global layout:
    /// byte offsets rebased, tops accumulated, alignment resolved against the
    /// final block width.
    ///
    /// This is the whole of "a block is many paragraphs": no reshaping, just
    /// addition — which is why an edit costs one paragraph, not the document.
    fn assemble(&self, style: &Style, parts: &[ParagraphKey]) -> Layout {
        let chain = self.chain_view(style.chain);
        let Some(primary) = chain.first() else {
            return Layout::default();
        };
        let metrics = primary.font.metrics();
        let line_height_em = metrics.line_height() * style.line_spacing;
        let ascent_em = metrics.ascent;

        let mut lines: Vec<LayoutLine> = Vec::new();
        let mut byte_offset = 0usize;
        let mut top_em = 0.0f32;
        let mut width_em = 0.0f32;

        for key in parts {
            let Some(para) = self.paragraphs.get(&(*key, *style)) else {
                continue;
            };
            for flow in &para.lines {
                width_em = width_em.max(flow.advance);
                lines.push(LayoutLine {
                    byte_range: flow.source.start + byte_offset..flow.source.end + byte_offset,
                    metrics: LineMetrics {
                        top_em,
                        baseline_em: top_em + ascent_em,
                        height_em: line_height_em,
                        width_em: flow.advance,
                    },
                    carets: flow
                        .carets
                        .iter()
                        .map(|caret| CaretStop {
                            byte_index: caret.byte_index + byte_offset,
                            x_em: caret.x_em,
                        })
                        .collect(),
                    align_em: 0.0,
                    glyphs: flow.glyphs.clone(),
                });
                top_em += line_height_em;
            }
            // Paragraphs are joined by a newline, so the next one's offsets start
            // one byte past this one's end.
            byte_offset += para.len_bytes + 1;
        }

        // Alignment needs the final block width, so it resolves here rather than
        // during flow.
        let block_width = style.wrap_em.unwrap_or(width_em).max(width_em);
        for line in &mut lines {
            line.align_em = match style.align {
                Align::Left => 0.0,
                Align::Center => (block_width - line.metrics.width_em) * 0.5,
                Align::Right => block_width - line.metrics.width_em,
            };
        }

        let height_em = lines
            .last()
            .map(|line| line.metrics.top_em + line.metrics.height_em)
            .unwrap_or(0.0);
        Layout {
            lines,
            width_em,
            height_em,
        }
    }

    /// Drop least-recently-used entries once a pool is over its bound. A block
    /// the consumer keeps drawing is kept alive by being re-shaped (a comparison
    /// when nothing moved), so this only ever reaches genuinely cold entries.
    fn evict(&mut self) {
        if self.paragraphs.len() > MAX_PARAGRAPHS {
            let mut ages: Vec<u64> = self.paragraphs.values().map(|p| p.last_used).collect();
            let cut = self.paragraphs.len() - MAX_PARAGRAPHS * 3 / 4;
            ages.select_nth_unstable(cut);
            let threshold = ages[cut];
            self.paragraphs.retain(|_, entry| entry.last_used > threshold);
        }

        let live = self.blocks.len() - self.free_blocks.len();
        if live <= MAX_BLOCKS {
            return;
        }
        let mut ages: Vec<u64> = self
            .blocks
            .iter()
            .filter_map(|slot| slot.block.as_ref().map(|block| block.last_used))
            .collect();
        let cut = live - MAX_BLOCKS * 3 / 4;
        ages.select_nth_unstable(cut);
        let threshold = ages[cut];
        for (index, slot) in self.blocks.iter_mut().enumerate() {
            let cold = slot
                .block
                .as_ref()
                .is_some_and(|block| block.last_used <= threshold);
            if cold {
                if let Some(block) = slot.block.take() {
                    self.block_lookup.remove(&block.key);
                    self.free_blocks.push(index as u32);
                }
            }
        }
    }
}

/// The chain as borrows into the font pool. A free function so callers can split
/// the service's fields around it.
fn chain_view<'a>(
    fonts: &'a [Font],
    chains: &[Option<Vec<FontHandle>>],
    chain: FontChainHandle,
) -> Vec<ChainFont<'a>> {
    chains
        .get(chain.0 as usize)
        .and_then(Option::as_ref)
        .map(|handles| {
            handles
                .iter()
                .filter_map(|handle| {
                    fonts
                        .get(handle.0 as usize)
                        .map(|font| ChainFont { id: handle.0, font })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn glyph_intersects(info: &GlyphInfo, pen_x: f32, pen_y: f32, size: f32, clip: Rect) -> bool {
    let (min_x, min_y, max_x, max_y) = info.bbox;
    let left = pen_x + min_x * size;
    let right = pen_x + max_x * size;
    let top = pen_y - max_y * size;
    let bottom = pen_y - min_y * size;
    right >= clip.x && left <= clip.max_x() && bottom >= clip.y && top <= clip.max_y()
}

// ---------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------

/// Read-only introspection: font coverage and cache occupancy.
pub struct Diagnostics<'a> {
    text: &'a Text,
}

impl Diagnostics<'_> {
    /// Family names of every face in a chain, in fallback order.
    pub fn chain_families(&self, chain: FontChainHandle) -> Vec<String> {
        self.text
            .chain_view(chain)
            .iter()
            .map(|entry| {
                entry
                    .font
                    .family_name()
                    .unwrap_or_else(|| "<unknown>".to_string())
            })
            .collect()
    }

    /// Characters in `text` that no face in `chain` covers — i.e. what will
    /// render as tofu.
    pub fn uncovered_chars(&self, chain: FontChainHandle, text: &str) -> Vec<char> {
        let view = self.text.chain_view(chain);
        let mut missing: Vec<char> = text
            .chars()
            .filter(|c| {
                !c.is_whitespace()
                    && !c.is_control()
                    && !view.iter().any(|entry| entry.font.has_glyph(*c))
            })
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    /// Whether any face in `chain` covers `c`. `false` means tofu.
    pub fn covers(&self, chain: FontChainHandle, c: char) -> bool {
        self.text
            .chain_view(chain)
            .iter()
            .any(|entry| entry.font.has_glyph(c))
    }

    /// Family name of the face `c` would actually resolve to.
    pub fn family_for(&self, chain: FontChainHandle, c: char) -> Option<String> {
        let view = self.text.chain_view(chain);
        let entry = view.iter().find(|entry| entry.font.has_glyph(c))?;
        entry.font.family_name()
    }

    /// Em-space bounding box `(min_x, min_y, max_x, max_y)` of `c`'s outline, for
    /// laying a single glyph out precisely. `None` for a color glyph or a
    /// character nothing covers.
    pub fn glyph_bbox(&self, chain: FontChainHandle, c: char) -> Option<(f32, f32, f32, f32)> {
        let view = self.text.chain_view(chain);
        let entry = view.iter().find(|entry| entry.font.has_glyph(c))?;
        let id = entry.font.face().glyph_index(c)?;
        let outlines = entry.font.load_glyph(id)?;
        Some(outlines.bounding_box())
    }

    /// Whether `text` shapes to exactly one glyph in this chain — i.e. the font
    /// ligates the whole sequence (a flag, a ZWJ emoji) rather than rendering it
    /// as pieces.
    pub fn is_single_glyph(&self, chain: FontChainHandle, text: &str) -> bool {
        let view = self.text.chain_view(chain);
        view.iter()
            .any(|entry| crate::layout::shapes_to_single_glyph(entry.font, text))
    }

    /// Curve, band and emoji atlas sizes, in texels.
    pub fn atlas_sizes(&self) -> ((u32, u32), (u32, u32), (u32, u32)) {
        (
            self.text.glyphs.curve_size(),
            self.text.glyphs.band_size(),
            self.text.emoji.size(),
        )
    }

    /// Color glyphs dropped because the emoji atlas was full.
    pub fn dropped_glyphs(&self) -> u64 {
        self.text.emoji.dropped_glyphs()
    }

    /// `(cached paragraph layouts, live blocks)`.
    pub fn cache_occupancy(&self) -> (usize, usize) {
        (
            self.text.paragraphs.len(),
            self.text
                .blocks
                .iter()
                .filter(|slot| slot.block.is_some())
                .count(),
        )
    }
}
