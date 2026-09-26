//! The public surface: one `TextService`, `Copy` handles into its pools.
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
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use crate::cache::{GlyphCache, GlyphInfo};
use crate::emoji::{EmojiCache, bucket_for};
use crate::flow::{FlowLine, flow_paragraph};
use crate::font::Font;
use crate::layout::{ChainFont, ShapedGlyph, ShapedRun, shape_spanned, shape_text};
use crate::renderer::{EmojiPage, EmojiRenderer, TextAtlas, TextRenderer, Uniforms};
use crate::spans::{
    FontSpan, PaintCursor, PaintError, PaintHandle, PaintPool, PaintSpan, valid_font_spans,
};
use crate::vertex::{EmojiVertex, TextVertex, push_emoji_quad, push_glyph_quad_pixels};

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

/// Linear RGBA. A draw parameter only — never baked into shaping or the atlases,
/// so a recolor reshapes and rasterizes nothing. It *is* baked into cached
/// geometry (per-vertex color is what lets differently-colored blocks share one
/// draw call), so a recolor re-emits that block's quads: a CPU walk, not a
/// reshape.
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
    /// The font or chain pool has exhausted its 65,535 slots.
    PoolFull,
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse => write!(f, "failed to parse font"),
            Self::PoolFull => write!(f, "font or chain pool is full"),
        }
    }
}

impl std::error::Error for FontError {}

// ---------------------------------------------------------------------------
// handles and keys
// ---------------------------------------------------------------------------

/// One mapped concrete font, local to its service. Deduped by data identity.
/// Invalid after `clear`; remap rather than reusing an old font handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontHandle(u16);

/// An ordered fallback chain of fonts, local to its service.
/// Released handles cannot select or release a later occupant of the slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontChainHandle {
    slot: u16,
    generation: u32,
}

struct ChainSlot {
    generation: u32,
    fonts: Option<Vec<FontHandle>>,
}

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

impl ShapedHandle {
    /// An unallocated sentinel that measures empty and draws nothing.
    pub const INVALID: Self = Self {
        slot: u32::MAX,
        generation: 0,
    };
}

/// The consumer's identity for one paragraph: the unit of *invalidation*.
/// Its generation covers text **and effective font spans**, never paint.
///
/// `namespace` keeps two documents' pool slots from colliding in the shared
/// cache. It stays a separate field rather than being hashed into `slot` because
/// a collision here renders the *wrong text*, silently and persistently.
/// These fields impose no allocation scheme: a globally unique consumer handle
/// may be split losslessly across namespace/slot. All namespace values are usable.
/// Allocation identity alone is not an edit version; text/font-span changes must
/// change the full key. The service keeps this key separate from transient text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParagraphKey {
    pub namespace: u64,
    pub slot: u32,
    pub generation: u32,
}

/// The consumer's identity for a composed block: the unit of *coordinate space*.
///
/// Carries no version — change detection compares the parts and style. Reusing
/// this key reshapes the same mutable layout instance; simultaneous different
/// layouts of the same content require different keys. Paragraph namespaces do
/// not namespace block keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey(pub u64);

/// Base font and paragraph layout policy. Inline font spans are separate source
/// inputs covered by the paragraph generation. No pixels or color: moving the
/// camera re-runs nothing.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub chain: FontChainHandle,
    /// Wrap width in em (`pane_px / font_px`), or `None` for no wrapping.
    pub wrap_em: Option<f32>,
    pub align: Align,
    /// Multiplier on the font's metric line height.
    pub line_spacing: f32,
}

impl PartialEq for Style {
    fn eq(&self, other: &Self) -> bool {
        self.chain == other.chain
            && self.wrap_em.map(f32::to_bits) == other.wrap_em.map(f32::to_bits)
            && self.align == other.align
            && self.line_spacing.to_bits() == other.line_spacing.to_bits()
    }
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

/// A placed caret: a byte offset **and** the visual line it is shown on.
///
/// The line is not derivable from the byte: at a soft break one byte belongs
/// to two visual lines, and which one the caret renders on is the affinity the
/// user's last action decided. Carrying both makes that decision impossible to
/// drop — [`Layout::caret_move`] consumes and produces `Caret`s, so the
/// ambiguity never leaks into consumer bookkeeping. After a reshape the line
/// index may be stale; [`Layout::clamp_caret`] re-anchors it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caret {
    pub byte_index: usize,
    pub line_index: usize,
}

/// One caret motion, resolved by [`Layout::caret_move`].
///
/// Everything here is pure layout geometry except `WordLeft`/`WordRight`,
/// which classify *text* the service never holds — they consult the caller's
/// [`WordBoundaries`]. `PageUp`/`PageDown` carry their stride in visual lines,
/// because page size is viewport knowledge, which is also the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp(usize),
    PageDown(usize),
    DocStart,
    DocEnd,
    WordLeft,
    WordRight,
}

/// Word classification over the caller's text, for [`Motion::WordLeft`] /
/// [`Motion::WordRight`]. Words are semantics, not shaping, so the crate asks
/// rather than guesses — the same seam as [`ParagraphSource`]. Return `None`
/// to decline; the motion degrades to a cluster step. `()` always declines.
pub trait WordBoundaries {
    fn prev_word(&self, byte_index: usize) -> Option<usize>;
    fn next_word(&self, byte_index: usize) -> Option<usize>;
}

impl WordBoundaries for () {
    fn prev_word(&self, _: usize) -> Option<usize> {
        None
    }
    fn next_word(&self, _: usize) -> Option<usize> {
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CaretRect {
    pub x_em: f32,
    pub y_em: f32,
    pub height_em: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct SelectionSpan {
    pub line_index: usize,
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
    /// Paragraph origin for block-local paint lookup. Keep it per line rather
    /// than rebasing every copied glyph on the plain-text assembly path.
    paragraph_byte: usize,
}

/// Laid-out geometry for one block, in em space, with block-global byte offsets
/// across all of its paragraphs.
///
/// Borrow this from [`TextService::measure`] **at the point of use** — do not store it.
/// Every query returns `Copy` or owned data precisely so nothing needs to outlive
/// the call, which is what keeps `&mut self` free for [`TextService::draw`]. Pass the
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
    /// loading a font or touching a GPU. Lines must be in visual/logical LTR
    /// order, with coherent byte ranges, metrics and caret stops (including line
    /// endpoints). Hard paragraph seams have a separator byte; soft seams share
    /// an endpoint. This low-level fixture constructor does not validate inputs.
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
                    paragraph_byte: 0,
                })
                .collect(),
            width_em,
            height_em,
        }
    }

    /// Widest natural line advance, em, not the wrap box or aligned ink bounds.
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
    pub fn hit_test(&self, at_em: Vec2) -> Option<Caret> {
        let line_index = self.line_index_for_y(at_em.y)?;
        let byte_index = self.caret_byte_on_line(line_index, at_em.x)?;
        Some(Caret {
            byte_index,
            line_index,
        })
    }

    /// The line a byte offset falls on. A byte at a soft break belongs to the
    /// line that *starts* with it, which is what word-wrap caret affinity needs.
    fn line_for_byte(&self, byte_index: usize) -> Option<usize> {
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

    /// Byte index of the nearest caret stop on `line_index` to `x_em`.
    pub fn caret_byte_on_line(&self, line_index: usize, x_em: f32) -> Option<usize> {
        let line = self.lines.get(line_index)?;
        let local_x = x_em - line.align_em;
        line.carets
            .iter()
            .min_by(|a, b| {
                (a.x_em - local_x)
                    .abs()
                    .total_cmp(&(b.x_em - local_x).abs())
            })
            .map(|caret| caret.byte_index)
    }

    /// Rectangle of a placed caret, in em. Re-anchor with [`Self::clamp_caret`]
    /// after reshaping; this query preserves the supplied visual-line affinity.
    /// An invalid line returns an empty rectangle. A byte outside that line
    /// projects to its start/end, useful for block-caret advance measurements.
    pub fn caret_rect(&self, caret: Caret) -> CaretRect {
        let Some(line) = self.lines.get(caret.line_index) else {
            return CaretRect { x_em: 0.0, y_em: 0.0, height_em: 0.0 };
        };
        CaretRect {
            x_em: caret_x_on(line, caret.byte_index) + line.align_em,
            y_em: line.metrics.top_em,
            height_em: line.metrics.height_em,
        }
    }

    /// Highlight rects for a byte range, one per covered line.
    /// The caret stop after `byte_index` in document order, if any.
    ///
    /// Cluster-true Left/Right stepping for an editor: stops come from shaping,
    /// so they can't land inside a ligature or a ZWJ emoji sequence the way
    /// external grapheme segmentation can. At a hard paragraph break the last
    /// stop of one line and the first of the next straddle the newline byte,
    /// which is exactly the step an editor wants.
    pub fn next_caret_stop(&self, byte_index: usize) -> Option<usize> {
        self.lines
            .iter()
            .flat_map(|line| line.carets.iter())
            .map(|caret| caret.byte_index)
            .filter(|&byte| byte > byte_index)
            .min()
    }

    /// The caret stop before `byte_index` in document order, if any.
    pub fn prev_caret_stop(&self, byte_index: usize) -> Option<usize> {
        self.lines
            .iter()
            .flat_map(|line| line.carets.iter())
            .map(|caret| caret.byte_index)
            .filter(|&byte| byte < byte_index)
            .max()
    }

    /// Re-anchor a caret to this layout: byte clamped into range, line kept if
    /// it still contains the byte (that *is* the affinity), else re-derived.
    /// Call after a reshape invalidates line indices.
    pub fn clamp_caret(&self, caret: Caret) -> Caret {
        let byte_index = caret.byte_index.min(self.len_bytes());
        let line_index = self
            .line_range(caret.line_index)
            .filter(|range| byte_index >= range.start && byte_index <= range.end)
            .map(|_| caret.line_index)
            .unwrap_or_else(|| self.caret_at(byte_index).line_index);
        Caret {
            byte_index,
            line_index,
        }
    }

    /// The caret for a byte, with the layout's default (start-affine) line.
    ///
    /// Hard-break separator bytes stay on the line they terminate, including
    /// empty paragraphs. At soft wraps, use [`Self::caret_after_edit`] for
    /// end-affine placement or retain the line returned by hit-testing/motion.
    pub fn caret_at(&self, byte_index: usize) -> Caret {
        let byte_index = byte_index.min(self.len_bytes());
        let natural = self.line_for_byte(byte_index).unwrap_or(0);
        let line_index = if self
            .line_range(natural)
            .is_some_and(|range| range.start > byte_index)
        {
            natural.saturating_sub(1)
        } else {
            natural
        };
        Caret {
            byte_index,
            line_index,
        }
    }

    /// The caret for a byte placed by an **edit**: end-affine at a soft break,
    /// so typing the character that wraps a line leaves the caret shown at the
    /// end of the line it was typed on rather than the start of the next.
    pub fn caret_after_edit(&self, byte_index: usize) -> Caret {
        let caret = self.caret_at(byte_index);
        if caret.line_index > 0
            && self
                .line_range(caret.line_index)
                .is_some_and(|range| range.start == caret.byte_index)
            && self
                .line_range(caret.line_index - 1)
                .is_some_and(|range| range.end == caret.byte_index)
        {
            return Caret {
                line_index: caret.line_index - 1,
                ..caret
            };
        }
        caret
    }

    /// Resolve one caret [`Motion`]. Consumes a placed caret, returns the next
    /// one with its affinity decided — the bookkeeping every editor needs and
    /// keeps getting wrong, written once, next to the geometry it reads.
    ///
    /// `goal` is the vertical-motion goal column (em), owned by the caller so
    /// the service stays stateless: vertical motions seed and preserve it,
    /// every other motion clears it. Pass the same `&mut Option<f32>` you keep
    /// beside the caret. `text` supplies word boundaries ([`WordBoundaries`]); pass
    /// `&()` to degrade word motions to cluster steps.
    ///
    /// The affinity rules, in one place: a horizontal step landing on the
    /// current line's start or end stays on the current line; a byte that is a
    /// hard break's end (which `line_for_byte` gives to the following line)
    /// pins to the line whose end it is; Up on the top line snaps to its
    /// start, Down on the bottom line to its end.
    pub fn caret_move(
        &self,
        caret: Caret,
        motion: Motion,
        goal: &mut Option<f32>,
        text: &(impl WordBoundaries + ?Sized),
    ) -> Caret {
        if self.lines.is_empty() {
            *goal = None;
            return Caret {
                byte_index: 0,
                line_index: 0,
            };
        }
        let caret = self.clamp_caret(caret);

        // Horizontal placement: boundary bytes keep the line the caret came
        // from; elsewhere `caret_at`'s default resolution.
        let place = |byte_index: usize| -> Caret {
            let byte_index = byte_index.min(self.len_bytes());
            if let Some(range) = self.line_range(caret.line_index) {
                if byte_index == range.start || byte_index == range.end {
                    return Caret {
                        byte_index,
                        line_index: caret.line_index,
                    };
                }
            }
            self.caret_at(byte_index)
        };

        let vertical = |goal: &mut Option<f32>, up: bool, lines: usize| -> Caret {
            if lines == 0 { return caret; }
            let current = caret.line_index;
            let target = if up {
                current.saturating_sub(lines)
            } else {
                current.saturating_add(lines).min(self.lines.len() - 1)
            };
            if target == current {
                // Boundary line: Up snaps to its start, Down to its end.
                let range = self.line_range(current);
                let byte_index = if up {
                    range.map(|r| r.start).unwrap_or(0)
                } else {
                    range.map(|r| r.end).unwrap_or_else(|| self.len_bytes())
                };
                return Caret {
                    byte_index,
                    line_index: current,
                };
            }
            let x = *goal.get_or_insert_with(|| {
                self.caret_rect(caret).x_em
            });
            let byte_index = self.caret_byte_on_line(target, x).unwrap_or(caret.byte_index);
            Caret {
                byte_index,
                line_index: target,
            }
        };

        match motion {
            Motion::Left => {
                *goal = None;
                place(
                    self.prev_caret_stop(caret.byte_index)
                        .unwrap_or(caret.byte_index),
                )
            }
            Motion::Right => {
                *goal = None;
                place(
                    self.next_caret_stop(caret.byte_index)
                        .unwrap_or(caret.byte_index),
                )
            }
            Motion::WordLeft => {
                *goal = None;
                place(text.prev_word(caret.byte_index).unwrap_or_else(|| {
                    self.prev_caret_stop(caret.byte_index)
                        .unwrap_or(caret.byte_index)
                }))
            }
            Motion::WordRight => {
                *goal = None;
                place(text.next_word(caret.byte_index).unwrap_or_else(|| {
                    self.next_caret_stop(caret.byte_index)
                        .unwrap_or(caret.byte_index)
                }))
            }
            Motion::Home => {
                *goal = None;
                Caret {
                    byte_index: self
                        .line_range(caret.line_index)
                        .map(|range| range.start)
                        .unwrap_or(0),
                    line_index: caret.line_index,
                }
            }
            Motion::End => {
                *goal = None;
                Caret {
                    byte_index: self
                        .line_range(caret.line_index)
                        .map(|range| range.end)
                        .unwrap_or_else(|| self.len_bytes()),
                    line_index: caret.line_index,
                }
            }
            Motion::Up => vertical(goal, true, 1),
            Motion::Down => vertical(goal, false, 1),
            Motion::PageUp(lines) => vertical(goal, true, lines),
            Motion::PageDown(lines) => vertical(goal, false, lines),
            Motion::DocStart => {
                *goal = None;
                Caret {
                    byte_index: 0,
                    line_index: 0,
                }
            }
            Motion::DocEnd => {
                *goal = None;
                Caret {
                    byte_index: self.len_bytes(),
                    line_index: self.lines.len() - 1,
                }
            }
        }
    }

    /// The word around `byte_index` — the double-click selection. Classified
    /// by the caller's [`WordBoundaries`] (words are semantics, not shaping); a
    /// declining classifier degrades to the cluster around the byte.
    pub fn select_word_at(&self, byte_index: usize, text: &(impl WordBoundaries + ?Sized)) -> Range<usize> {
        let byte_index = byte_index.min(self.len_bytes());
        let start = text
            .prev_word(byte_index)
            .unwrap_or_else(|| self.prev_caret_stop(byte_index).unwrap_or(byte_index));
        let end = text
            .next_word(byte_index)
            .unwrap_or_else(|| self.next_caret_stop(byte_index).unwrap_or(byte_index));
        start.min(byte_index)..end.max(byte_index)
    }

    /// The paragraph around `byte_index` — the triple-click selection: the
    /// range between the hard breaks on either side. Pure geometry, unlike
    /// [`Layout::select_word_at`]: a block's hard breaks *are* its paragraph
    /// seams by construction (parts are joined with a separator byte), so no
    /// classifier is consulted.
    pub fn select_paragraph_at(&self, byte_index: usize) -> Range<usize> {
        let byte_index = byte_index.min(self.len_bytes());
        let line = self.caret_at(byte_index).line_index;
        let soft_joined = |a: usize, b: usize| -> bool {
            match (self.line_range(a), self.line_range(b)) {
                (Some(upper), Some(lower)) => upper.end == lower.start,
                _ => false,
            }
        };
        let mut first = line;
        while first > 0 && soft_joined(first - 1, first) {
            first -= 1;
        }
        let mut last = line;
        while last + 1 < self.lines.len() && soft_joined(last, last + 1) {
            last += 1;
        }
        let start = self.line_range(first).map(|r| r.start).unwrap_or(0);
        let end = self
            .line_range(last)
            .map(|r| r.end)
            .unwrap_or_else(|| self.len_bytes());
        start..end
    }

    pub fn selection(&self, range: Range<usize>) -> Vec<SelectionSpan> {
        if range.is_empty() {
            return Vec::new();
        }
        /// Visual width of a selected line break, em: the stub an editor shows
        /// past the last glyph — and the entire highlight of a blank line.
        const NEWLINE_STUB_EM: f32 = 0.45;
        let mut spans = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            let start = range.start.max(line.byte_range.start);
            let end = range.end.min(line.byte_range.end);
            // A hard break's separator byte belongs to no line. Selecting past
            // this line's end selects it, and that must be *visible*: a stub
            // after the last glyph, which on a blank line is the whole span.
            // Soft wraps have no separator byte and get no stub — the highlight
            // simply continues on the next line.
            let hard_break = self
                .lines
                .get(index + 1)
                .is_some_and(|next| next.byte_range.start > line.byte_range.end);
            let newline_selected =
                hard_break && range.start <= line.byte_range.end && range.end > line.byte_range.end;
            if start >= end && !newline_selected {
                continue;
            }
            let x0 = caret_x_on(line, start.min(end));
            let x1 = caret_x_on(line, end.max(start));
            let stub = if newline_selected {
                NEWLINE_STUB_EM
            } else {
                0.0
            };
            spans.push(SelectionSpan {
                line_index: index,
                x_em: x0.min(x1) + line.align_em,
                y_em: line.metrics.top_em,
                width_em: (x1 - x0).abs() + stub,
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
/// [`TextService::shape`] and of anything storing it, which then unifies with the
/// caller's other borrows.
///
/// This is not a heavyweight provider: `&self`, used as `&dyn`, never a generic.
///
/// `index` comes alongside the key because `shape` calls this only for parts that
/// *miss* — an implementor cannot assume one in-order call per part, so one
/// holding its text positionally needs the index to find it.
pub trait ParagraphSource {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>>;

    /// Inline font inputs, consulted only on the same cache misses as text.
    /// Sorted, nonoverlapping, nonempty grapheme-safe ranges; gaps use the base
    /// style. Invalid ranges or unavailable chains make `shape` return `None`.
    /// Bump the paragraph generation when these effective inputs change, even
    /// when text is unchanged. Color-only changes belong to a paint snapshot.
    fn paragraph_fonts(&self, _index: usize, _key: ParagraphKey) -> Cow<'_, [FontSpan]> {
        Cow::Borrowed(&[])
    }
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

struct ParaShape {
    run: ShapedRun,
    span_chains: Box<[FontChainHandle]>,
    last_used: u64,
}

/// Level-2 result for one paragraph at one style.
struct ParaLayout {
    span_chains: Box<[FontChainHandle]>,
    lines: Vec<FlowLine>,
    len_bytes: usize,
    last_used: u64,
}

struct BlockSlot {
    block: Option<Block>,
    /// Bumped on every (re)assembly. An edit reshapes a block **in place** —
    /// same key, same slot, same handle — so a retained [`Batch`] cannot see
    /// it in anything it holds; this is what `batch_live` checks instead.
    revision: u64,
}

// Consumer identities and content keys are disjoint domains. Keep the full
// keys: HashMap resolves hash collisions by equality, not by trusting a digest
// as identity. Only the identity-free path retains text as cache-key material.
#[derive(Clone, PartialEq, Eq, Hash)]
enum BlockIdentity {
    Named(BlockKey),
    Transient(Arc<str>, Style),
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum ParagraphIdentity {
    Named(ParagraphKey),
    Transient(Arc<str>),
}

struct Block {
    key: BlockIdentity,
    style: Style,
    parts: Vec<ParagraphIdentity>,
    // Retained independently of paragraph-cache residency for drop_chain.
    span_chains: Box<[FontChainHandle]>,
    layout: Layout,
    last_used: u64,
}

/// What the cached vertices were built for. Color is baked per-vertex — the
/// price of letting differently-colored blocks share one draw call — so it is
/// part of the key. `at` and `size` are deliberately absent: quads bake at the
/// origin and unit size, and `prepare` applies both on the way out.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GeomKey {
    color: [u32; 4],
    paint: Option<PaintHandle>,
    /// Normalised into the block's own space, `(clip - at) / size`, because the
    /// quads are. An absolute clip would reintroduce exactly the position and
    /// scale dependence this key exists without: under a camera move `at`, `size`
    /// and the scissor all change together, and this ratio does not.
    clip: Option<[u32; 4]>,
}

/// A block's quads, kept on the **CPU**.
///
/// Deliberately not a GPU buffer per block: quads live in the transform's source
/// space, so they survive pan and zoom, and keeping them host-side lets many
/// blocks concatenate into one upload and one draw call. A buffer per block
/// means a draw call per block, which is ruinous on a dense canvas.
/// Quads are baked with the block's top-left at the **origin**; `prepare`
/// adds `at` as it concatenates. This is what lets one shaped block be drawn at
/// many positions — a repeated label, a glyph reused across a grid, or a body
/// scrolling under a fixed clip — from a single build. Baking `at` in instead
/// means a block drawn at N positions rebuilds N times per frame, each rebuild
/// discarding the last.
struct Geometry {
    key: GeomKey,
    text: Vec<TextVertex>,
    emoji: Vec<EmojiRequest>,
}

/// A color glyph's place in the geometry, not a cache-dependent atlas address.
/// The text prefix records interleaving without another allocation for plain text.
struct EmojiRequest {
    font: u16,
    glyph: u32,
    pen: Vec2,
    text_before: usize,
}

/// One block to draw, for [`TextService::draw_batch`].
// `PartialEq` so a consumer retaining a `Batch` can compare this frame's draw
// list against the one it prepared from, instead of hand-rolling the compare.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Draw {
    pub block: ShapedHandle,
    /// Top-left of the block box, in the transform's source space.
    pub at: Vec2,
    pub size: f32,
    pub color: Color,
    pub clip: Option<Rect>,
    /// Optional immutable foreground snapshot. A stale handle skips this item
    /// on prepare, even if old CPU geometry exists. Already-prepared batches
    /// own their baked colors and do not depend on paint-pool lifetime.
    pub paint: Option<PaintHandle>,
}

/// No registration/allocation: invalid block, origin (0,0), size 1, opaque black,
/// and no paint or clip. Supply a real `block` with ordinary struct-update syntax.
impl Default for Draw {
    fn default() -> Self {
        Self {
            block: ShapedHandle::INVALID,
            at: Vec2::default(),
            size: 1.,
            color: Color([0., 0., 0., 1.]),
            clip: None,
            paint: None,
        }
    }
}

/// One clip-uniform run inside a [`Batch`]: the vertices between two scissor
/// changes. Read-only, minted by [`TextService::prepare`], dies with its batch.
///
/// The general draw loop is: map `clip` to framebuffer pixels, set the scissor,
/// [`TextService::draw_segment`]. The mapping is the consumer's one job in that
/// loop — under an arbitrary transform only the consumer knows the viewport,
/// which is why the crate contains no scissor call.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    /// The clip these items were prepared with, echoed back in the space it was
    /// passed in. `None` = unclipped: no scissor needed beyond the pass's own.
    pub clip: Option<Rect>,
    runs: (usize, usize),
}

/// Blocks the consumer chose to group, concatenated into one GPU buffer **the
/// consumer holds** — the unit of vertex ownership (see the "`Batch` owns its
/// vertices" lock in `decisions.md`).
///
/// The vertex buffer is owned; emoji pages are shared but append-only. Evicting
/// a page from the service's cache never overwrites it. Dropping a batch after
/// recording is safe: wgpu retains bound resources through submission. Retained
/// pages may outlive the cache's 64 MiB residency budget; dropping batches is how
/// the consumer releases that ownership. An unchanged live batch costs no upload.
/// After `clear`, stale batches are memory-safe but not preserved snapshots of
/// monochrome atlas contents. Check [`TextService::batch_live`] before reuse.
///
/// Order within a batch is the order of the `&[Draw]` it was prepared from —
/// that is z-order, and it is the consumer's: `prepare` never reorders, and
/// drawing segments out of order is a silent z bug the service cannot detect.
pub struct Batch {
    buffer: wgpu::Buffer,
    segments: Vec<Segment>,
    runs: Vec<DrawRun>,
    split: u64,
    total: u64,
    /// Only color glyphs depend on a requested raster bucket, never plain text.
    emoji_sizes: Vec<(f32, u32)>,
    /// A prepare that skipped stale inputs must not become a permanently live
    /// empty replacement. The caller needs to re-shape/re-register those inputs.
    complete: bool,
    /// The blocks these vertices were baked from, as `(slot, generation,
    /// revision)`. An edit reshapes a block in place — same key, slot and
    /// handle — so nothing in the consumer's draw list changes; the revision
    /// is how `batch_live` sees it. Adjacent duplicates are collapsed.
    blocks: Vec<(u32, u32, u64)>,
}

impl Batch {
    /// Where the clip changes: one entry per scissor-uniform run, in draw
    /// order. A nonempty uniform-clip request has exactly one.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }
}

struct DrawRun {
    /// None selects Slug. Some owns the append-only emoji page being sampled.
    page: Option<Arc<EmojiPage>>,
    vertices: Range<u32>,
}

struct Gpu {
    device: wgpu::Device,
    format: wgpu::TextureFormat,
    pipelines: HashMap<wgpu::TextureFormat, (TextRenderer, EmojiRenderer)>,
    uniforms: Uniforms,
    text_layout: wgpu::BindGroupLayout,
    emoji_layout: wgpu::BindGroupLayout,
    text_atlas: TextAtlas,
}

impl Gpu {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, matrix: [f32; 16]) -> Self {
        let uniforms = Uniforms::new(device, matrix);
        let text_layout = TextRenderer::atlas_layout(device);
        let emoji_layout = EmojiRenderer::atlas_layout(device);
        let text_atlas = TextAtlas::empty(device, &text_layout);
        let mut gpu = Self {
            device: device.clone(), format, pipelines: HashMap::new(),
            uniforms, text_layout, emoji_layout, text_atlas,
        };
        gpu.set_target(format);
        gpu
    }

    fn set_target(&mut self, format: wgpu::TextureFormat) {
        self.pipelines.entry(format).or_insert_with(|| (
            TextRenderer::for_format(&self.device, format, &self.uniforms.layout, &self.text_layout),
            EmojiRenderer::for_format(&self.device, format, &self.uniforms.layout, &self.emoji_layout),
        ));
        self.format = format;
    }
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
/// device until [`Self::set_target`]. Shaping, measurement and hit-testing remain
/// GPU-free. GPU resources belong to one device; use a new service for a new
/// device. All font/chain/paint/shaped handles and batches are service-local.
#[derive(Default)]
pub struct TextService {
    fonts: Vec<Font>,
    chains: Vec<ChainSlot>,
    free_chains: Vec<u16>,

    blocks: Vec<BlockSlot>,
    /// Slot generations, held apart from `blocks` so validating a handle touches
    /// a dense 4-byte array instead of pulling a ~200-byte `BlockSlot` into cache.
    generations: Vec<u32>,
    /// Pool 7, also held apart from `blocks`. `prepare` walks this and nothing
    /// else: on a dense frame it is the only array in the hot loop, and keeping it
    /// off `Block` is the difference between touching ~96 bytes per item and
    /// chasing through the block's layout, parts and style to reach it.
    geometry: Vec<Option<Geometry>>,
    /// Tombstoned block slots, newest first. Allocation pops from here instead of
    /// scanning `blocks` for a hole — a linear scan per `shape` is quadratic over
    /// a frame that shapes thousands of small blocks. Slots are never moved, so
    /// live handles stay valid.
    free_blocks: Vec<u32>,
    block_lookup: HashMap<BlockIdentity, ShapedHandle>,
    shaped: HashMap<(ParagraphIdentity, FontChainHandle), ParaShape>,
    paragraphs: HashMap<(ParagraphIdentity, Style), ParaLayout>,

    paints: PaintPool,
    glyphs: GlyphCache,
    emoji: EmojiCache,
    gpu: Option<Gpu>,
    transform: Option<[f32; 16]>,
    /// On-screen pixels per source-space unit; `None` = 1 (exact under
    /// `pixel_ortho`). Consulted only when picking an emoji raster bucket —
    /// see [`TextService::set_pixel_scale`].
    pixel_scale: Option<f32>,

    clock: u64,
    empty_layout: Layout,
    batch_text: Vec<TextVertex>,
    batch_emoji: Vec<EmojiVertex>,
}

impl TextService {
    /// Takes nothing: pipelines are per-target and lazy (see [`TextService::set_target`]),
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
        self.map_font_with_variations(data, face_index, &[])
    }

    /// Map an immutable variable-font instance. Tags are four-byte OpenType axis
    /// names, such as `*b"wght"`; values use the font's design coordinates.
    /// Unknown axes are ignored and values clamp to the font's supported range.
    /// Shaping, metrics and glyph outlines use the same instance. Deduplication
    /// includes normalized axis coordinates as well as shared data and face index.
    pub fn map_font_with_variations(&mut self, data: FontData, face_index: u32, variations: &[([u8; 4], f32)]) -> Result<FontHandle, FontError> {
        if variations.iter().any(|(_, value)| !value.is_finite()) { return Err(FontError::Parse); }
        let font = Font::from_shared(data, face_index, variations)?;
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
    /// defines the line metrics for anything shaped with it. At most 65,535 slots;
    /// exhaustion returns [`FontError::PoolFull`]. Dropped slots are reused with a
    /// new generation; exhausted generations are retired instead of wrapping.
    pub fn register_chain(&mut self, fonts: &[FontHandle]) -> Result<FontChainHandle, FontError> {
        let slot = if let Some(slot) = self.free_chains.pop() {
            slot
        } else {
            if self.chains.len() >= u16::MAX as usize {
                return Err(FontError::PoolFull);
            }
            self.chains.push(ChainSlot { generation: 1, fonts: None });
            (self.chains.len() - 1) as u16
        };
        let entry = &mut self.chains[slot as usize];
        entry.fonts = Some(fonts.to_vec());
        Ok(FontChainHandle { slot, generation: entry.generation })
    }

    fn release_chain(&mut self, chain: FontChainHandle) -> bool {
        let Some(entry) = self.chains.get_mut(chain.slot as usize) else { return false; };
        if entry.generation != chain.generation || entry.fonts.take().is_none() {
            return false;
        }
        // Retire exhausted slots instead of reviving an ancient handle.
        if let Some(next) = entry.generation.checked_add(1) {
            entry.generation = next;
            self.free_chains.push(chain.slot);
        }
        true
    }

    /// Drop a chain and every block and paragraph shaped with it.
    ///
    /// Fonts stay in the pool — another chain may share them — but nothing keeps
    /// the old chain's layouts alive. This is what a settings font change needs,
    /// and what the old `Box::leak`ed font bytes made impossible. Releasing a stale
    /// or already-released handle is a no-op, even after slot reuse or clear.
    pub fn drop_chain(&mut self, chain: FontChainHandle) {
        if !self.release_chain(chain) {
            return;
        }
        self.paragraphs
            .retain(|(_, style), para| style.chain != chain && !para.span_chains.contains(&chain));
        self.shaped
            .retain(|(_, base), para| *base != chain && !para.span_chains.contains(&chain));
        for index in 0..self.blocks.len() {
            let stale = self.blocks[index].block.as_ref().is_some_and(|block| {
                block.style.chain == chain || block.span_chains.contains(&chain)
            });
            if stale {
                self.remove_block(index);
            }
        }
    }

    // -- immutable foreground paint ------------------------------------------

    /// Copy a sorted, nonoverlapping foreground snapshot into the paint pool.
    /// No interning or implicit eviction. Keep the old handle for unchanged
    /// spans; independent equal registrations remain different draw inputs.
    pub fn register_paint(&mut self, spans: &[PaintSpan]) -> Result<PaintHandle, PaintError> {
        self.paints.register(spans)
    }

    /// Release a snapshot. Future prepares with this handle skip the item;
    /// retained batches still contain their baked colors. No layout invalidation.
    /// Exhausted slot generations are retired rather than wrapping.
    pub fn drop_paint(&mut self, paint: PaintHandle) {
        self.paints.drop(paint);
    }

    /// Drop every font, chain, paint snapshot and cached layout. Old shaped
    /// handles and batches remain stale after new layouts are allocated.
    ///
    /// Pipelines, transform, and monochrome atlas allocations are retained; the
    /// next prepare uploads replacement contents. Emoji cache ownership is
    /// released, not overwritten: batches/recorded draws keep their old pages
    /// alive and new color glyphs allocate fresh pages. Slot generation history
    /// survives. Recreate fonts/chains/paint and re-shape before preparing again.
    pub fn clear(&mut self) {
        self.fonts.clear();
        for slot in 0..self.chains.len() {
            self.release_chain(FontChainHandle {
                slot: slot as u16, generation: self.chains[slot].generation,
            });
        }
        for index in 0..self.blocks.len() {
            self.remove_block(index);
        }
        self.shaped.clear();
        self.paragraphs.clear();
        self.paints.clear();
        self.glyphs = GlyphCache::new();
        self.emoji.clear();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.text_atlas.invalidate_contents();
        }
    }

    // -- shaping (GPU-free) --------------------------------------------------

    /// Shape a block: 1..N paragraphs flowed together, with block-global byte
    /// offsets (paragraphs joined by a newline, exactly as the consumer's
    /// document reads).
    ///
    /// `source` is consulted only for parts that miss the cache; `None` from it
    /// means a stale identity and the whole block is skipped. Re-calling with an
    /// unchanged `parts` slice at the same style is a comparison, not a reflow,
    /// while the block remains cached. A camera transform does not require this
    /// call at all.
    ///
    /// A new width, alignment, or line spacing reflows paragraphs but reuses
    /// their shaped glyphs. Changing one paragraph at the same style reuses
    /// the others' cached results; block assembly still copies all paragraphs'
    /// glyphs and carets.
    pub fn shape(
        &mut self,
        block: BlockKey,
        style: &Style,
        parts: &[ParagraphKey],
        source: &dyn ParagraphSource,
    ) -> Option<ShapedHandle> {
        self.shape_block(
            BlockIdentity::Named(block),
            style,
            parts.iter().copied().map(ParagraphIdentity::Named),
            Some(source),
        )
    }

    fn shape_block(
        &mut self,
        block: BlockIdentity,
        style: &Style,
        parts: impl Iterator<Item = ParagraphIdentity> + Clone,
        source: Option<&dyn ParagraphSource>,
    ) -> Option<ShapedHandle> {
        crate::work::count!(block_requests, 1);
        self.clock += 1;
        self.chain_fonts(style.chain)?;

        // Unchanged parts at the same style: the block is already correct, so
        // this is a comparison rather than a reflow.
        if let Some(&handle) = self.block_lookup.get(&block) {
            if let Some(index) = self.block_index(handle) {
                if let Some(existing) = self.blocks[index].block.as_mut() {
                    // A transient block's full text and style already identify
                    // its parts. Do not allocate paragraph keys on a warm hit.
                    if existing.style == *style
                        && (matches!(&block, BlockIdentity::Transient(..))
                            || existing.parts.iter().cloned().eq(parts.clone()))
                    {
                        crate::work::count!(block_hits, 1);
                        existing.last_used = self.clock;
                        return Some(handle);
                    }
                }
            }
        }

        let parts: Vec<_> = parts.collect();
        if parts.is_empty() {
            return None;
        }
        for (index, key) in parts.iter().enumerate() {
            self.ensure_paragraph(key, style, index, source)?;
        }
        let (layout, span_chains) = self.assemble(style, &parts);

        // Reuse this block's own slot if it still holds one; otherwise take a
        // free slot and bump its generation so any handle to the old occupant
        // stops resolving.
        let index = match self.block_lookup.get(&block) {
            Some(handle) if (handle.slot as usize) < self.blocks.len() => handle.slot as usize,
            _ => match self.free_blocks.pop() {
                Some(slot) => slot as usize,
                None => {
                    self.blocks.push(BlockSlot {
                        block: None,
                        revision: 0,
                    });
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
            key: block.clone(),
            style: *style,
            parts,
            span_chains,
            layout,
            last_used: self.clock,
        });
        self.blocks[index].revision = self.blocks[index].revision.wrapping_add(1);
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

    /// Shape text with no stable consumer identity — a tooltip or a label.
    ///
    /// Cached by full text **and style**, so two requests with different wrapping
    /// or fonts coexist. Unlike [`TextService::shape`], an edit names a different
    /// block rather than updating one in place. Newlines split the text into
    /// paragraphs; their content keys exclude width, preserving shaping reuse.
    /// Text is copied into cache keys and retained while those entries are cached.
    /// These keys cannot alias consumer-supplied block or paragraph identities.
    pub fn shape_transient(&mut self, text: &str, style: &Style) -> Option<ShapedHandle> {
        self.shape_block(
            BlockIdentity::Transient(Arc::from(text), *style),
            style,
            text.split('\n')
                .map(|line| ParagraphIdentity::Transient(Arc::from(line))),
            None,
        )
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

    /// Select a single-sample color target. Pipelines are cached per format;
    /// changing format preserves atlases, transforms, and retained batches.
    /// Call before preparing/drawing. A service's GPU resources belong to this
    /// device; create a new service to replace the device. No depth testing.
    pub fn set_target(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        if let Some(gpu) = &mut self.gpu {
            assert_eq!(&gpu.device, device, "TextService belongs to a different device");
            gpu.set_target(format);
        } else {
            self.gpu = Some(Gpu::new(device, format,
                self.transform.unwrap_or(glam::Mat4::IDENTITY.to_cols_array())));
        }
    }

    /// Set the column-major transform for subsequent draws. Works before GPU
    /// initialization. Each changed matrix has immutable GPU storage: earlier
    /// recorded passes keep their matrix even before a shared submission.
    /// Screen space uses [`Self::pixel_ortho`]; world/3D text uses an MVP.
    pub fn set_transform(&mut self, transform: [f32; 16]) {
        self.transform = Some(transform);
        if let Some(gpu) = &mut self.gpu {
            gpu.uniforms.set(&gpu.device, transform);
        }
    }

    /// On-screen pixels per source-space unit. Consulted only to pick the emoji
    /// raster bucket; Slug text is analytic and needs no bucket at all.
    ///
    /// Under [`TextService::pixel_ortho`] the default (1) is exact and this never
    /// needs calling. Under an MVP, `size` is in world units and the service
    /// cannot know what one unit maps to on screen — state it (a camera zoom,
    /// typically), updating alongside [`TextService::set_transform`] when it
    /// changes, or emoji rasterize at world-unit resolution and blur under
    /// magnification. `batch_live` becomes false when a batch's requested emoji
    /// bucket changes; monochrome-only batches stay live. Zero and negatives are
    /// ignored.
    pub fn set_pixel_scale(&mut self, px_per_unit: f32) {
        if px_per_unit > 0.0 {
            self.pixel_scale = Some(px_per_unit);
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

    /// Record one [`Draw`], including optional paint, through the batch path.
    /// Coordinates are in the transform's source space; `at` is the block's
    /// top-left. `clip` culls whole glyphs; set a pass scissor for hard clipping.
    pub fn draw(
        &mut self, device: &wgpu::Device, queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>, item: Draw,
    ) {
        self.draw_batch(device, queue, pass, &[item]);
    }

    /// Build a [`Batch`] the consumer owns: every block's quads (cached, and
    /// reused across frames while its draw parameters hold) concatenated into
    /// one buffer, split into [`Segment`]s where the clip changes.
    ///
    /// Geometry rebuilds, emoji rasterization/page uploads, monochrome atlas
    /// sync and vertex upload happen here. `draw_segment`/`draw_prepared` purely
    /// record. The batch keeps its emoji pages even if later preparation evicts
    /// them; cached CPU geometry stores glyph requests, never stale texture UVs.
    /// Panics if no target was selected or `device` differs from its owner.
    ///
    /// Input order is preserved (it is z-order, and it is yours); adjacent
    /// items with an equal clip coalesce into one segment. Retention is only
    /// meaningful if `at`/`size` are stable across frames — under a camera,
    /// pass world units with the camera in the transform (`set_transform`),
    /// not pre-projected pixels, or every pan invalidates every batch.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, items: &[Draw]) -> Batch {
        crate::work::count!(prepares, 1);
        crate::work::count!(prepared_items, items.len());
        let gpu = self.gpu.as_ref().expect("call set_target before prepare");
        assert_eq!(&gpu.device, device, "TextService belongs to a different device");
        let pixel_scale = self.pixel_scale.unwrap_or(1.0);
        let mut text_verts = std::mem::take(&mut self.batch_text);
        let mut emoji_verts = std::mem::take(&mut self.batch_emoji);
        text_verts.clear();
        emoji_verts.clear();
        text_verts.reserve(items.len() * 6);
        let mut segments = Vec::new();
        let mut runs = Vec::new();
        let mut blocks = Vec::new();
        let mut emoji_sizes = Vec::new();
        let mut complete = true;
        for clip_run in clip_runs(items) {
            let run_start = runs.len();
            for item in &items[clip_run.clone()] {
                let Some(index) = self.block_index(item.block) else {
                    complete = false;
                    continue;
                };
                if item.paint.is_some_and(|h| self.paints.get(h).is_none()) {
                    complete = false;
                    continue;
                }
                let baked = (item.block.slot, item.block.generation, self.blocks[index].revision);
                if blocks.last() != Some(&baked) { blocks.push(baked); }
                let key = GeomKey {
                    color: item.color.0.map(f32::to_bits), paint: item.paint,
                    clip: normalized_clip(item).map(|c| [c.x, c.y, c.width, c.height].map(f32::to_bits)),
                };
                if self.geometry[index].as_ref().is_some_and(|g| g.key == key) {
                    crate::work::count!(geometry_hits, 1);
                } else {
                    self.rebuild_geometry(index, item, key);
                }
                let geom = self.geometry[index].as_ref().expect("geometry built");
                let bucket = bucket_for(item.size * pixel_scale);
                if !geom.emoji.is_empty() && emoji_sizes.last() != Some(&(item.size, bucket)) {
                    emoji_sizes.push((item.size, bucket));
                }
                let gpu = self.gpu.as_ref().expect("target set");
                let mut cursor = 0;
                for request in &geom.emoji {
                    let start = text_verts.len() as u32;
                    place_text(&mut text_verts, &geom.text[cursor..request.text_before], item.at, item.size);
                    push_run(&mut runs, run_start, None, start..text_verts.len() as u32);
                    cursor = request.text_before;
                    let font = &self.fonts[request.font as usize];
                    if let Some(slot) = self.emoji.get_or_insert(
                        font.face(), request.font, request.glyph, bucket, device, queue, &gpu.emoji_layout,
                    ) {
                        let start = emoji_verts.len() as u32;
                        crate::work::count!(emoji_quads, 1);
                        push_emoji_quad(&mut emoji_verts,
                            request.pen.x * item.size + item.at.x,
                            request.pen.y * item.size + item.at.y, item.size,
                            [slot.x as f32, slot.y as f32],
                            [(slot.x + slot.size) as f32, (slot.y + slot.size) as f32]);
                        push_run(&mut runs, run_start, Some(slot.page), start..emoji_verts.len() as u32);
                    }
                }
                let start = text_verts.len() as u32;
                place_text(&mut text_verts, &geom.text[cursor..], item.at, item.size);
                push_run(&mut runs, run_start, None, start..text_verts.len() as u32);
            }
            segments.push(Segment { clip: items[clip_run.start].clip, runs: (run_start, runs.len()) });
        }
        let gpu = self.gpu.as_mut().expect("target set");
        gpu.text_atlas.sync(device, queue, &gpu.text_layout, &self.glyphs);
        let text_data: &[u8] = bytemuck::cast_slice(&text_verts);
        let emoji_data: &[u8] = bytemuck::cast_slice(&emoji_verts);
        let split = text_data.len() as u64;
        let total = split + emoji_data.len() as u64;
        crate::work::count!(vertex_upload_bytes, total);
        crate::work::count!(prepared_segments, segments.len());
        let buffer = batch_buffer(device, total);
        if !text_data.is_empty() { queue.write_buffer(&buffer, 0, text_data); }
        if !emoji_data.is_empty() { queue.write_buffer(&buffer, split, emoji_data); }
        self.batch_text = text_verts;
        self.batch_emoji = emoji_verts;
        Batch { buffer, segments, runs, split, total, emoji_sizes, complete, blocks }
    }

    /// Record one clip-uniform segment in input order, interleaving Slug and
    /// emoji page runs. Sets no scissor: set yours first from [`Segment::clip`].
    /// Recording only: no allocation, upload, or cache mutation. An out-of-range
    /// index or an unset target is a no-op.
    pub fn draw_segment(&self, pass: &mut wgpu::RenderPass<'_>, batch: &Batch, index: usize) {
        let Some(gpu) = &self.gpu else { return; };
        let Some(segment) = batch.segments.get(index) else { return; };
        let (text, emoji) = &gpu.pipelines[&gpu.format];
        for run in &batch.runs[segment.runs.0..segment.runs.1] {
            if let Some(page) = &run.page {
                emoji.draw(pass, &gpu.uniforms.binding, page, &batch.buffer,
                    batch.split..batch.total, run.vertices.clone());
            } else {
                text.draw_vertices(pass, &gpu.uniforms.binding, &gpu.text_atlas, &batch.buffer,
                    0..batch.split, run.vertices.clone());
            }
        }
    }

    /// Record every segment of a prepared batch, in order, with no scissor
    /// changes — the uniform-clip fast path, and what [`TextService::draw_batch`]
    /// desugars to. For per-segment scissors, loop [`Batch::segments`] and
    /// [`TextService::draw_segment`] yourself.
    pub fn draw_prepared(&self, pass: &mut wgpu::RenderPass<'_>, batch: &Batch) {
        for index in 0..batch.segments.len() {
            self.draw_segment(pass, batch, index);
        }
    }

    /// Whether a batch's layouts and requested emoji resolution are current.
    /// Caller-owned draw inputs (position, color, clip, paint) must also match.
    /// Emoji cache eviction cannot change its pixels: the batch owns its pages.
    ///
    /// On `false`, re-issue `shape`/`shape_transient` from your source/style and
    /// replace the draw handles before preparing. Re-preparing alone cannot
    /// recover an evicted handle. A prepare that skipped invalid block/paint
    /// handles remains non-live; it cannot cache missing text as a live result.
    /// Already-baked paint colors survive `drop_paint`, but a new prepare needs
    /// a live paint handle. After `clear`, recreate fonts/chains/paint as well.
    pub fn batch_live(&self, batch: &Batch) -> bool {
        batch.complete
            && batch.emoji_sizes.iter().all(|&(size, bucket)|
                bucket_for(size * self.pixel_scale.unwrap_or(1.0)) == bucket)
            && batch.blocks.iter().all(|&(slot, generation, revision)| {
                self.generations.get(slot as usize) == Some(&generation)
                    && self.blocks.get(slot as usize).is_some_and(|b| b.revision == revision)
            })
    }

    /// `prepare` + `draw_prepared` + drop: the easy path is the same rendering
    /// route. Adjacent compatible runs coalesce; each scissor, shader-kind or
    /// emoji-page transition can require another GPU draw. Input order wins
    /// over regrouping by pipeline. Plain uniform-clip text remains one draw.
    ///
    /// This is what a dense canvas needs: drawing tens of thousands of
    /// glyph-sized blocks one call each costs hundreds of milliseconds in
    /// command recording alone, and none of that work is about text.
    ///
    /// The batch is dropped as this returns; wgpu ref-counts what a pass binds,
    /// so its buffer outlives the recording without anyone holding it.
    pub fn draw_batch(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        items: &[Draw],
    ) {
        if items.is_empty() {
            return;
        }
        let batch = self.prepare(device, queue, items);
        self.draw_prepared(pass, &batch);
    }

    /// Build one block's quads into its geometry cache. CPU only — no device and
    /// no upload. Emoji requests retain their position in the text stream;
    /// `prepare` resolves them into owned page bindings and ordered GPU runs.
    fn rebuild_geometry(
        &mut self,
        index: usize,
        item: &Draw,
        key: GeomKey,
    ) {
        crate::work::count!(geometry_builds, 1);
        let color = item.color;
        let mut paint = item
            .paint
            .and_then(|h| self.paints.get(h))
            .map(|spans| PaintCursor::new(spans, color));
        // Bake at the origin *and* at unit size, with the clip normalised to
        // match, so the build depends on everything about this draw except where
        // it lands and how big it is. `prepare` applies both on the way out.
        let at = Vec2::new(0.0, 0.0);
        let size = 1.0f32;
        let clip = normalized_clip(item);
        let mut text_verts = Vec::new();
        let mut emoji_requests = Vec::new();
        {
            let block = self.blocks[index]
                .block
                .as_ref()
                .expect("checked by caller");
            for_each_visible_glyph(
                &block.layout,
                at,
                size,
                clip,
                |glyph, pen_x, pen_y, paragraph_byte| {
                    if glyph.is_color {
                        emoji_requests.push(EmojiRequest {
                            font: glyph.font_id, glyph: glyph.glyph_id,
                            pen: Vec2::new(pen_x, pen_y), text_before: text_verts.len(),
                        });
                    } else if let Some(info) = glyph.info {
                        crate::work::count!(text_quads, 1);
                        let foreground = paint
                            .as_mut()
                            .map(|p| p.at(paragraph_byte + glyph.cluster))
                            .unwrap_or(color);
                        push_glyph_quad_pixels(
                            &mut text_verts,
                            &info,
                            pen_x,
                            pen_y,
                            size,
                            foreground.0,
                        );
                    }
                },
            );
        }

        self.geometry[index] = Some(Geometry { key, text: text_verts, emoji: emoji_requests });
    }

    // -- internals -----------------------------------------------------------

    fn chain_fonts(&self, chain: FontChainHandle) -> Option<&[FontHandle]> {
        chain_fonts(&self.chains, chain)
    }

    fn chain_view(&self, chain: FontChainHandle) -> Vec<ChainFont<'_>> {
        chain_view(&self.fonts, &self.chains, chain)
    }

    /// Shape and flow one paragraph if it isn't already cached at this style.
    fn ensure_paragraph(
        &mut self,
        key: &ParagraphIdentity,
        style: &Style,
        index: usize,
        source: Option<&dyn ParagraphSource>,
    ) -> Option<()> {
        crate::work::count!(paragraph_requests, 1);
        let clock = self.clock;
        if let Some(entry) = self.paragraphs.get_mut(&(key.clone(), *style)) {
            crate::work::count!(paragraph_hits, 1);
            entry.last_used = clock;
            if let Some(shaped) = self.shaped.get_mut(&(key.clone(), style.chain)) {
                shaped.last_used = clock;
            }
            return Some(());
        }
        // Resolve bytes only on a paragraph miss. Named sources stay lazy;
        // identity-free paragraphs already own their content as cache keys.
        crate::work::count!(source_reads, 1);
        let text = match key {
            ParagraphIdentity::Named(key) => source?.paragraph_text(index, *key)?,
            ParagraphIdentity::Transient(text) => Cow::Borrowed(text.as_ref()),
        };
        crate::work::count!(source_bytes, text.len());
        if !self.shaped.contains_key(&(key.clone(), style.chain)) {
            let spans = match key {
                ParagraphIdentity::Named(key) => source?.paragraph_fonts(index, *key),
                ParagraphIdentity::Transient(_) => Cow::Borrowed(&[][..]),
            };
            if !valid_font_spans(&text, &spans) {
                return None;
            }
            crate::work::count!(font_spans, spans.len());
            let chain = chain_view(&self.fonts, &self.chains, style.chain);
            if chain.is_empty() {
                return None;
            }
            let mut dependencies = Vec::new();
            let run = if spans.is_empty() {
                shape_text(&chain, &mut self.glyphs, text.as_ref())
            } else {
                let mut views = vec![chain];
                let mut indices = HashMap::from([(style.chain, 0usize)]);
                let mut resolved = Vec::with_capacity(spans.len());
                for span in spans.iter() {
                    let next = views.len();
                    let position = *indices.entry(span.chain).or_insert_with(|| {
                        views.push(chain_view(&self.fonts, &self.chains, span.chain));
                        dependencies.push(span.chain);
                        next
                    });
                    if views[position].is_empty() {
                        return None;
                    }
                    resolved.push((span.range.clone(), position));
                }
                shape_spanned(&views, &resolved, &mut self.glyphs, text.as_ref())
            };
            self.shaped.insert(
                (key.clone(), style.chain),
                ParaShape {
                    run,
                    span_chains: dependencies.into_boxed_slice(),
                    last_used: clock,
                },
            );
        }
        let shaped = self.shaped.get_mut(&(key.clone(), style.chain))?;
        shaped.last_used = clock;
        let lines = flow_paragraph(text.as_ref(), &shaped.run.glyphs, style.max_width_em());
        self.paragraphs.insert(
            (key.clone(), *style),
            ParaLayout {
                span_chains: shaped.span_chains.clone(),
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
    /// Reuse paragraph shaping/flow, but copy all glyphs and carets into a new
    /// composed layout. A one-paragraph edit avoids reshaping the other paragraphs;
    /// assembly is still proportional to the whole block, not the edited range.
    fn assemble(
        &self,
        style: &Style,
        parts: &[ParagraphIdentity],
    ) -> (Layout, Box<[FontChainHandle]>) {
        crate::work::count!(assemblies, 1);
        let chain = self.chain_view(style.chain);
        let Some(primary) = chain.first() else {
            return (Layout::default(), Box::default());
        };
        let metrics = primary.font.metrics();
        let line_height_em = metrics.line_height() * style.line_spacing;
        let baseline_offset_em =
            metrics.ascent + (line_height_em - (metrics.ascent - metrics.descent)) * 0.5;

        let mut lines: Vec<LayoutLine> = Vec::new();
        let mut byte_offset = 0usize;
        let mut top_em = 0.0f32;
        let mut width_em = 0.0f32;

        let mut dependencies = HashSet::new();
        for key in parts {
            let Some(para) = self.paragraphs.get(&(key.clone(), *style)) else {
                continue;
            };
            dependencies.extend(para.span_chains.iter().copied());
            for flow in &para.lines {
                crate::work::count!(assembled_lines, 1);
                crate::work::count!(assembled_glyphs, flow.glyphs.len());
                crate::work::count!(assembled_carets, flow.carets.len());
                width_em = width_em.max(flow.advance);
                lines.push(LayoutLine {
                    byte_range: flow.source.start + byte_offset..flow.source.end + byte_offset,
                    metrics: LineMetrics {
                        top_em,
                        baseline_em: top_em + baseline_offset_em,
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
                    paragraph_byte: byte_offset,
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
        (
            Layout {
                lines,
                width_em,
                height_em,
            },
            dependencies.into_iter().collect(),
        )
    }

    /// One invalidation path for clear, chain release and eviction. Keep the
    /// generation history even when every block is gone: retained handles and
    /// batches must not become valid again when the slots are populated anew.
    fn remove_block(&mut self, index: usize) {
        if let Some(block) = self.blocks[index].block.take() {
            self.block_lookup.remove(&block.key);
            self.generations[index] = self.generations[index].wrapping_add(1);
            self.geometry[index] = None;
            self.free_blocks.push(index as u32);
        }
    }

    /// Drop least-recently-used entries once a pool is over its bound. A block
    /// the consumer keeps drawing is kept alive by being re-shaped (a comparison
    /// when nothing moved), so this only ever reaches genuinely cold entries.
    fn evict(&mut self) {
        if self.shaped.len() > MAX_PARAGRAPHS {
            let mut ages: Vec<u64> = self.shaped.values().map(|p| p.last_used).collect();
            let cut = self.shaped.len() - MAX_PARAGRAPHS * 3 / 4;
            ages.select_nth_unstable(cut);
            let threshold = ages[cut];
            self.shaped.retain(|_, entry| entry.last_used > threshold);
        }
        if self.paragraphs.len() > MAX_PARAGRAPHS {
            let mut ages: Vec<u64> = self.paragraphs.values().map(|p| p.last_used).collect();
            let cut = self.paragraphs.len() - MAX_PARAGRAPHS * 3 / 4;
            ages.select_nth_unstable(cut);
            let threshold = ages[cut];
            self.paragraphs.retain(|_, entry| {
                let keep = entry.last_used > threshold;
                crate::work::count!(paragraph_evictions, usize::from(!keep));
                keep
            });
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
        for index in 0..self.blocks.len() {
            let cold = self.blocks[index]
                .block
                .as_ref()
                .is_some_and(|block| block.last_used <= threshold);
            if cold {
                crate::work::count!(block_evictions, 1);
                self.remove_block(index);
            }
        }
    }
}

fn chain_fonts(chains: &[ChainSlot], chain: FontChainHandle) -> Option<&[FontHandle]> {
    let entry = chains.get(chain.slot as usize)?;
    (entry.generation == chain.generation).then_some(())?;
    entry.fonts.as_deref().filter(|fonts| !fonts.is_empty())
}

/// The chain as borrows into the font pool. A free function so callers can split
/// the service's fields around it.
fn chain_view<'a>(
    fonts: &'a [Font],
    chains: &[ChainSlot],
    chain: FontChainHandle,
) -> Vec<ChainFont<'a>> {
    chain_fonts(chains, chain)
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

/// Walk the glyphs of `layout` that survive `clip`, with their pen positions in
/// the transform's source space.
///
/// Split out of `rebuild_geometry` so the cull is reachable without a device.
/// It previously lived inline in the one function that needs a GPU to call, which
/// is why the three clip tests the old engine had could not be carried over — and
/// then the half of the clip contract that was never built went unnoticed for a
/// release. Culling is pure geometry; it should not need a GPU to test.
fn for_each_visible_glyph(
    layout: &Layout,
    at: Vec2,
    size: f32,
    clip: Option<Rect>,
    mut f: impl FnMut(&ShapedGlyph, f32, f32, usize),
) {
    for line in &layout.lines {
        crate::work::count!(visited_lines, 1);
        let top = at.y + line.metrics.top_em * size;
        let bottom = top + line.metrics.height_em * size;
        if clip.is_some_and(|clip| bottom < clip.y || top > clip.max_y()) {
            crate::work::count!(culled_lines, 1);
            continue;
        }
        crate::work::count!(visited_glyphs, line.glyphs.len());
        let baseline = at.y + line.metrics.baseline_em * size;
        let origin_x = at.x + line.align_em * size;
        for glyph in &line.glyphs {
            let pen_x = origin_x + glyph.x * size;
            let pen_y = baseline - glyph.y * size;
            if glyph.is_color {
                // A colour glyph is a size x size box hanging from the baseline;
                // it has no outline to measure, so the box is the bound.
                let outside = clip.is_some_and(|clip| {
                    pen_y < clip.y
                        || pen_y - size > clip.max_y()
                        || pen_x + size < clip.x
                        || pen_x > clip.max_x()
                });
                if !outside {
                    f(glyph, pen_x, pen_y, line.paragraph_byte);
                }
                continue;
            }
            let Some(info) = glyph.info else { continue };
            if clip.is_some_and(|clip| !glyph_intersects(&info, pen_x, pen_y, size, clip)) {
                continue;
            }
            f(glyph, pen_x, pen_y, line.paragraph_byte);
        }
    }
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
    text: &'a TextService,
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

    /// Non-control/non-whitespace scalars absent from every face's cmap.
    /// Coverage is not a guarantee about whole-cluster shaping or raster support.
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

    /// Whether any face maps `c` in its cmap, not a whole-cluster rendering test.
    pub fn covers(&self, chain: FontChainHandle, c: char) -> bool {
        self.text
            .chain_view(chain)
            .iter()
            .any(|entry| entry.font.has_glyph(c))
    }

    /// Family name of the face `c` would actually resolve to.
    ///
    /// Routed through the same `layout::face_for_grapheme` the shaper uses, so
    /// this cannot disagree with what gets drawn. Resolving
    /// it here independently — "first face with a glyph" — silently ignores
    /// emoji presentation and reports the color font for every text-presentation
    /// character it happens to cover.
    pub fn family_for(&self, chain: FontChainHandle, c: char) -> Option<String> {
        let view = self.text.chain_view(chain);
        let mut buf = [0u8; 4];
        let entry = view.get(crate::layout::face_for_grapheme(
            &view,
            c.encode_utf8(&mut buf),
        ))?;
        entry.font.has_glyph(c).then(|| entry.font.family_name())?
    }

    /// Em-space bounding box `(min_x, min_y, max_x, max_y)` of `c`'s outline, for
    /// laying a single glyph out precisely. `None` for a color glyph or a
    /// character nothing covers.
    pub fn glyph_bbox(&self, chain: FontChainHandle, c: char) -> Option<(f32, f32, f32, f32)> {
        let view = self.text.chain_view(chain);
        let mut buf = [0u8; 4];
        // The face the shaper picks, not the first one holding a glyph — this
        // feeds cell fit-scaling, so a wrong face moves geometry, not just text.
        let entry = view.get(crate::layout::face_for_grapheme(
            &view,
            c.encode_utf8(&mut buf),
        ))?;
        let id = entry.font.face().glyph_index(c)?;
        // A colour glyph reports nothing, as documented. COLR faces layer real
        // outlines under the colour, so asking for an outline *succeeds* and hands
        // back a box for something that will never be drawn that way.
        if entry.font.is_color_glyph(id.0) {
            return None;
        }
        let outlines = entry.font.load_glyph(id)?;
        Some(outlines.bounding_box())
    }

    /// Whether `text` shapes to exactly one glyph in this chain — i.e. the font
    /// ligates the whole sequence (a flag, a ZWJ emoji) rather than rendering it
    /// as pieces.
    /// Asks the *resolved* face, not any face in the chain. "Some face could
    /// ligate this" accepts a monochrome ligation the fallback walk rejects, so
    /// a caller drawing on that answer gets overlapping pieces instead of the
    /// one glyph — or the tofu — it was promised.
    pub fn is_single_glyph(&self, chain: FontChainHandle, text: &str) -> bool {
        let view = self.text.chain_view(chain);
        crate::layout::resolves_to_single_glyph(&view, text)
    }

    /// Curve/band atlas dimensions and the largest resident emoji page, in texels.
    pub fn atlas_sizes(&self) -> ((u32, u32), (u32, u32), (u32, u32)) {
        (
            self.text.glyphs.curve_size(),
            self.text.glyphs.band_size(),
            self.text.emoji.size(),
        )
    }

    /// Failed color-glyph rasterizations, counted once per cached failure key.
    /// Cache pressure evicts pages instead of dropping glyphs.
    pub fn dropped_glyphs(&self) -> u64 {
        self.text.emoji.dropped_glyphs()
    }

    /// `(resident emoji pages, texture bytes)` owned by the cache. Excludes pages
    /// kept alive only by retained batches or recorded GPU commands. Cache-owned
    /// pages have a 64 MiB budget; batch ownership is controlled by the consumer.
    pub fn emoji_cache_usage(&self) -> (usize, usize) {
        self.text.emoji.usage()
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

/// Split `items` into runs of equal `clip` — one [`Segment`] each.
///
/// **Adjacent only, and never reordered.** Order within a batch is z-order and
/// the consumer chose it, so grouping by clip identity — which would fold
/// `[a, b, a]` into two segments — silently reshuffles z. An interleaved clip
/// costs a segment, and a segment costs a scissor call, not an upload.
///
/// Free and pure so the rule that decides scissor boundaries is testable
/// without a device.
fn clip_runs(items: &[Draw]) -> Vec<Range<usize>> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if items[run.start].clip == item.clip => run.end = index + 1,
            _ => runs.push(index..index + 1),
        }
    }
    runs
}

/// The vertex buffer backing one [`Batch`]. Sized to the batch and never
/// suballocated or reused, which is the whole of the ownership fix: no shared
/// region means nothing to clobber.
fn batch_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    crate::work::count!(batch_buffers, 1);
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("sanscale batch vertices"),
        // A batch that placed nothing still owns a buffer; keep it nominally
        // sized rather than relying on zero-length buffers being legal.
        size: size.max(4),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn push_run(runs: &mut Vec<DrawRun>, segment_start: usize, page: Option<Arc<EmojiPage>>, vertices: Range<u32>) {
    if vertices.is_empty() { return; }
    if runs.len() > segment_start {
        let last = runs.last_mut().expect("run");
        let same = match (&last.page, &page) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same && last.vertices.end == vertices.start {
            last.vertices.end = vertices.end;
            return;
        }
    }
    runs.push(DrawRun { page, vertices });
}

/// Copy cached em-space vertices, applying the caller's source-space placement.
fn place_text(out: &mut Vec<TextVertex>, vertices: &[TextVertex], at: Vec2, size: f32) {
    let base = out.len();
    out.extend_from_slice(vertices);
    for v in &mut out[base..] {
        v.pos[0] = v.pos[0] * size + at.x;
        v.pos[1] = v.pos[1] * size + at.y;
    }
}

/// Grid the normalised clip snaps to, in em. Coarse enough to absorb rounding,
/// far finer than a glyph.
const CLIP_QUANTUM: f32 = 1.0 / 16.0;

/// A draw's clip in the block's own space: origin at the block's top-left, one
/// unit per em. This is the form that survives a camera move.
///
/// **Quantised outward, and it has to be.** `(clip - at) / size` is invariant
/// under a camera move only *analytically*: recomputed from a different zoom each
/// frame it lands a few ulps away, and a key compared by exact bits then misses
/// every single frame — which is precisely the bug this key was introduced to
/// fix, reintroduced one level down. Snapping to a power-of-two grid makes the
/// bits stable. Rounding *outward* keeps it safe: the clip only culls, so a
/// fractionally generous one emits a few extra quads the consumer's scissor
/// discards, where a tight one could clip a glyph that belonged on screen.
fn normalized_clip(item: &Draw) -> Option<Rect> {
    let q = CLIP_QUANTUM as f64;
    let inv = 1.0 / item.size as f64;
    // Nudge off the grid line before rounding. A clip that lands *exactly* on a
    // multiple of the quantum — the common case, since consumers use round
    // numbers — otherwise has `floor` flipping between two cells on a 1-ulp
    // wobble, which defeats the quantisation entirely. Both nudges push outward,
    // so the clip stays conservative.
    let bias = 1e-3;
    let snap_down = |v: f64| ((v * inv / q) - bias).floor() * q;
    let snap_up = |v: f64| ((v * inv / q) + bias).ceil() * q;
    item.clip.map(|c| {
        // Widen in f64: `clip - at` is a difference of similar magnitudes once the
        // camera is far from the origin, and doing it in f32 loses the precision
        // the ratio depends on.
        let (ax, ay) = (item.at.x as f64, item.at.y as f64);
        let x0 = snap_down(c.x as f64 - ax);
        let y0 = snap_down(c.y as f64 - ay);
        let x1 = snap_up((c.x + c.width) as f64 - ax);
        let y1 = snap_up((c.y + c.height) as f64 - ay);
        Rect::new(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::read_font_file;

    #[test]
    fn style_float_equality_is_bitwise() {
        let style = |wrap_em, line_spacing| Style {
            chain: FontChainHandle { slot: 0, generation: 0 },
            wrap_em,
            align: Align::Left,
            line_spacing,
        };
        assert_ne!(style(Some(0.0), 0.0), style(Some(-0.0), -0.0));

        let nan = style(Some(f32::NAN), f32::NAN);
        assert_eq!(nan, nan);
    }

    /// Three hard-broken lines — "ab" (0..2), blank (3..3), "cd" (4..6) — with
    /// unit-advance caret stops, GPU- and font-free via `from_lines`.
    fn three_line_layout() -> Layout {
        let line = |bytes: Range<usize>, top: f32| LayoutLineSpec {
            carets: bytes
                .clone()
                .chain([bytes.end])
                .enumerate()
                .map(|(i, byte_index)| CaretStop {
                    byte_index,
                    x_em: i as f32,
                })
                .collect(),
            metrics: LineMetrics {
                top_em: top,
                baseline_em: top + 0.8,
                height_em: 1.0,
                width_em: bytes.len() as f32,
            },
            byte_range: bytes,
        };
        Layout::from_lines(vec![line(0..2, 0.0), line(3..3, 1.0), line(4..6, 2.0)])
    }

    #[test]
    fn line_spacing_is_centered_around_the_font_box() {
        let (mut text, chain) = required_latin();
        let metrics = text.chain_view(chain)[0].font.metrics();
        let style = Style {
            line_spacing: 1.15,
            ..style_of(chain, None)
        };
        let handle = text
            .shape(BlockKey(1), &style, &span_keys(1), &Paragraphs(&["Ag"]))
            .unwrap();
        let layout = text.measure(handle);
        let line = layout.line(0).unwrap();
        let above = line.baseline_em - line.top_em - metrics.ascent;
        let below = line.top_em + line.height_em - (line.baseline_em - metrics.descent);
        assert!(above > 0.0);
        assert!((above - below).abs() < 1e-6);
        let caret = layout.caret_rect(layout.caret_at(0));
        let selected = &layout.selection(0..1)[0];
        assert_eq!((caret.y_em, caret.height_em), (line.top_em, line.height_em));
        assert_eq!(
            (selected.y_em, selected.height_em),
            (line.top_em, line.height_em)
        );
    }

    /// A selected hard newline shows as a stub past the last glyph, and a blank
    /// line inside the selection is a stub rather than nothing at all.
    #[test]
    fn selection_shows_newline_stubs_and_blank_lines() {
        let layout = three_line_layout();
        let spans = layout.selection(1..5);
        assert_eq!(
            spans.len(),
            3,
            "every line the selection touches has a span"
        );
        assert!(
            spans[0].width_em > 1.0,
            "line 0: one glyph plus the newline stub"
        );
        assert_eq!(spans[1].line_index, 1);
        assert!(
            spans[1].width_em > 0.0,
            "blank line: a visible stub, not nothing"
        );
        assert!(
            (spans[2].width_em - 1.0).abs() < 1e-6,
            "line 2: one glyph, newline not selected"
        );
        // Selection ending exactly at a line's end selects no newline: no stub.
        let spans = layout.selection(0..2);
        assert_eq!(spans.len(), 1);
        assert!((spans[0].width_em - 2.0).abs() < 1e-6);
    }

    /// Two soft-wrapped lines — "abcd|efg": 0..4 wraps to 4..7 — unit advances.
    fn wrapped_layout() -> Layout {
        let line = |bytes: Range<usize>, top: f32| LayoutLineSpec {
            carets: bytes
                .clone()
                .chain([bytes.end])
                .enumerate()
                .map(|(i, byte_index)| CaretStop {
                    byte_index,
                    x_em: i as f32,
                })
                .collect(),
            metrics: LineMetrics {
                top_em: top,
                baseline_em: top + 0.8,
                height_em: 1.0,
                width_em: bytes.len() as f32,
            },
            byte_range: bytes,
        };
        Layout::from_lines(vec![line(0..4, 0.0), line(4..7, 1.0)])
    }

    /// The affinity rules `caret_move` owns: boundary bytes keep the caret's
    /// line, verticals snap at the edges, and the goal column survives.
    #[test]
    fn caret_move_owns_the_affinity_rules() {
        let layout = wrapped_layout();
        let mut goal = None;

        // Right onto the soft break keeps the current line (end of line 0)...
        let caret = layout.caret_at(3);
        let caret = layout.caret_move(caret, Motion::Right, &mut goal, &());
        assert_eq!((caret.byte_index, caret.line_index), (4, 0));
        // ...and Left from inside line 1 onto the same byte keeps line 1.
        let caret = layout.caret_move(
            Caret {
                byte_index: 5,
                line_index: 1,
            },
            Motion::Left,
            &mut goal,
            &(),
        );
        assert_eq!((caret.byte_index, caret.line_index), (4, 1));

        // Up on the top line snaps to its start; Down on the bottom to its end.
        let caret = layout.caret_move(layout.caret_at(2), Motion::Up, &mut goal, &());
        assert_eq!((caret.byte_index, caret.line_index), (0, 0));
        let caret = layout.caret_move(
            Caret {
                byte_index: 5,
                line_index: 1,
            },
            Motion::Down,
            &mut goal,
            &(),
        );
        assert_eq!((caret.byte_index, caret.line_index), (7, 1));

        // The goal column seeds on the first vertical and survives the trip:
        // down from x=3 onto a 3-wide line clamps, up returns to x=3.
        let mut goal = None;
        let caret = layout.caret_move(layout.caret_at(3), Motion::Down, &mut goal, &());
        assert_eq!(caret.line_index, 1);
        assert_eq!(goal, Some(3.0));
        let caret = layout.caret_move(caret, Motion::Up, &mut goal, &());
        assert_eq!((caret.byte_index, caret.line_index), (3, 0));
        // Any horizontal motion clears it.
        layout.caret_move(caret, Motion::Left, &mut goal, &());
        assert_eq!(goal, None);

        // Word motions degrade to cluster steps under `()`.
        let mut goal = None;
        let caret = layout.caret_move(layout.caret_at(2), Motion::WordRight, &mut goal, &());
        assert_eq!(caret.byte_index, 3);
    }

    /// `select_word_at` composes the caller's boundaries; `()` degrades to the
    /// cluster around the byte.
    #[test]
    fn select_word_composes_boundaries_and_degrades_to_clusters() {
        struct Stub;
        impl WordBoundaries for Stub {
            fn prev_word(&self, _: usize) -> Option<usize> {
                Some(0)
            }
            fn next_word(&self, _: usize) -> Option<usize> {
                Some(4)
            }
        }
        let layout = wrapped_layout();
        assert_eq!(layout.select_word_at(2, &Stub), 0..4);
        // `()` declines: the selection is the cluster around the byte.
        assert_eq!(layout.select_word_at(2, &()), 1..3);
    }

    /// `select_paragraph_at` expands across soft wraps and stops at hard breaks.
    #[test]
    fn select_paragraph_spans_wraps_and_stops_at_hard_breaks() {
        // Soft-wrapped: both visual lines are one paragraph.
        assert_eq!(wrapped_layout().select_paragraph_at(6), 0..7);
        // Hard-broken: each line is its own paragraph, including the blank one.
        let hard = three_line_layout();
        assert_eq!(hard.select_paragraph_at(1), 0..2);
        assert_eq!(hard.select_paragraph_at(3), 3..3);
        assert_eq!(hard.select_paragraph_at(5), 4..6);
    }

    /// An edit reshapes a block **in place** — same key, same slot, same
    /// handle — so nothing in a consumer's retained draw list changes; the
    /// slot revision is the only witness, and `batch_live` reads it. Found
    /// live in compendium: typed text didn't render until something else
    /// perturbed the retention cache.
    #[test]
    fn reshape_in_place_bumps_the_revision_a_batch_checks() {
        let Some((mut text, chain)) = latin_chain() else {
            return;
        };
        let style = Style {
            chain,
            wrap_em: None,
            align: Align::Left,
            line_spacing: 1.0,
        };
        let key = |generation| ParagraphKey {
            namespace: 9,
            slot: 1,
            generation,
        };
        let h1 = text
            .shape(BlockKey(77), &style, &[key(0)], &Paragraphs(&["one"]))
            .expect("shaped");
        let baked = text.blocks[h1.slot as usize].revision;

        // Unchanged parts: a comparison, not a reshape — revision holds.
        text.shape(BlockKey(77), &style, &[key(0)], &Paragraphs(&["one"]));
        assert_eq!(text.blocks[h1.slot as usize].revision, baked);

        // Edited paragraph: same block key resolves to the same slot and
        // generation (the handle a consumer retained stays valid) but the
        // revision moves — which is what flips `batch_live`.
        let h2 = text
            .shape(BlockKey(77), &style, &[key(1)], &Paragraphs(&["two"]))
            .expect("reshaped");
        assert_eq!((h1.slot, h1.generation), (h2.slot, h2.generation));
        assert_ne!(text.blocks[h2.slot as usize].revision, baked);
    }

    /// `caret_after_edit` is end-affine at a soft break and natural elsewhere.
    #[test]
    fn caret_after_edit_is_end_affine_at_soft_breaks() {
        let layout = wrapped_layout();
        assert_eq!(layout.caret_after_edit(4).line_index, 0);
        assert_eq!(layout.caret_after_edit(2).line_index, 0);
        assert_eq!(layout.caret_after_edit(5).line_index, 1);
        // Hard break (three_line_layout): byte 3 starts the blank line and is
        // no line's soft end, so it stays natural.
        let hard = three_line_layout();
        assert_eq!(hard.caret_after_edit(3).line_index, 1);
    }

    fn font(paths: &[&str]) -> Option<FontData> {
        paths
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .and_then(|p| read_font_file(p).ok())
    }

    /// Build a chain with the colour font **first**, which is how a real fallback
    /// chain is ordered (emoji high-priority, Latin primary before it).
    fn emoji_first_chain() -> Option<(TextService, FontChainHandle)> {
        let emoji = font(&["C:/Windows/Fonts/seguiemj.ttf"])?;
        let latin = font(&[
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/arial.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/Library/Fonts/Arial.ttf",
        ])?;
        let mut text = TextService::new();
        let e = text.map_font(emoji, 0).ok()?;
        let l = text.map_font(latin, 0).ok()?;
        let chain = text.register_chain(&[e, l]).expect("font chain capacity");
        Some((text, chain))
    }

    fn latin_chain() -> Option<(TextService, FontChainHandle)> {
        let latin = font(&[
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/arial.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/Library/Fonts/Arial.ttf",
        ])?;
        let mut text = TextService::new();
        let h = text.map_font(latin, 0).ok()?;
        let chain = text.register_chain(&[h]).expect("font chain capacity");
        Some((text, chain))
    }

    fn style_of(chain: FontChainHandle, wrap_em: Option<f32>) -> Style {
        Style {
            chain,
            wrap_em,
            align: Align::Left,
            line_spacing: 1.0,
        }
    }

    /// A `Draw` that differs from its neighbours only in `clip` — enough for
    /// the segmentation rule, which reads nothing else. No font, no device.
    fn clipped(clip: Option<Rect>) -> Draw {
        Draw {
            block: ShapedHandle {
                slot: 0,
                generation: 0,
            },
            at: Vec2::new(0.0, 0.0),
            size: 16.0,
            color: Color([1.0, 1.0, 1.0, 1.0]),
            clip,
            ..Default::default()
        }
    }

    /// A segment is a scissor change, and a scissor that didn't change isn't
    /// one. Splitting per item instead would be correct and quietly cost a
    /// draw call per block — the thing batching exists to avoid.
    #[test]
    fn adjacent_equal_clips_coalesce() {
        let clip = Some(Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(
            clip_runs(&[clipped(clip), clipped(clip), clipped(clip)]),
            vec![0..3]
        );
        assert_eq!(clip_runs(&[clipped(None), clipped(None)]), vec![0..2]);
        assert_eq!(clip_runs(&[]), Vec::<Range<usize>>::new());
    }

    /// The consumer has nowhere to set a scissor *inside* a segment, so any
    /// change of clip — including to or from unclipped — has to end one.
    #[test]
    fn a_clip_change_starts_a_new_segment() {
        let a = Some(Rect::new(0.0, 0.0, 10.0, 10.0));
        let taller = Some(Rect::new(0.0, 0.0, 10.0, 11.0));
        let items = [clipped(a), clipped(taller), clipped(None), clipped(a)];
        assert_eq!(clip_runs(&items), vec![0..1, 1..2, 2..3, 3..4]);
    }

    /// Order within a batch is z-order and it is the consumer's. Grouping by
    /// clip *identity* would fold the two `a` runs below into one segment and
    /// silently draw `b` last — a reordering the service cannot detect and the
    /// consumer never asked for.
    #[test]
    fn segments_preserve_input_order() {
        let a = Some(Rect::new(0.0, 0.0, 1.0, 1.0));
        let b = Some(Rect::new(5.0, 5.0, 1.0, 1.0));
        let items = [clipped(a), clipped(a), clipped(b), clipped(a)];
        let runs = clip_runs(&items);
        assert_eq!(runs, vec![0..2, 2..3, 3..4]);
        // Every item lands in exactly one run, and the runs read in input order.
        let covered: Vec<usize> = runs.iter().flat_map(Clone::clone).collect();
        assert_eq!(covered, (0..items.len()).collect::<Vec<_>>());
    }

    /// Counts how often the service asks for a paragraph's bytes.
    struct CountingSource<'a> {
        text: &'a str,
        calls: std::cell::Cell<usize>,
    }

    impl ParagraphSource for CountingSource<'_> {
        fn paragraph_text(&self, _index: usize, _key: ParagraphKey) -> Option<Cow<'_, str>> {
            self.calls.set(self.calls.get() + 1);
            Some(Cow::Borrowed(self.text))
        }
    }

    /// Text is pulled from the consumer **only on a miss**.
    ///
    /// This is the escape-hatch contract that makes a 10k-paragraph document
    /// affordable: for a body where nothing changed, the text is never
    /// materialised. Restored from `engine.rs`, which the redesign deleted along
    /// with the module.
    #[test]
    fn a_cache_hit_never_asks_for_the_text() {
        let Some((mut text, chain)) = latin_chain() else {
            return;
        };
        let style = style_of(chain, Some(12.0));
        let key = ParagraphKey {
            namespace: 7,
            slot: 11,
            generation: 13,
        };
        let src = CountingSource {
            text: "cached paragraph should only ever be fetched once",
            calls: std::cell::Cell::new(0),
        };

        assert!(text.shape(BlockKey(1), &style, &[key], &src).is_some());
        assert_eq!(src.calls.get(), 1, "the first shape must fetch");

        // Same block, same parts: the block-level comparison short-circuits.
        text.shape(BlockKey(1), &style, &[key], &src);
        assert_eq!(
            src.calls.get(),
            1,
            "re-shaping an unchanged block must not fetch"
        );

        // A *different* block over the same paragraph: the block is new, so this
        // reaches the paragraph pool — which must still hit.
        text.shape(BlockKey(2), &style, &[key], &src);
        assert_eq!(src.calls.get(), 1, "a paragraph-cache hit must not fetch");

        // Bumping the generation is what invalidation looks like, and must fetch.
        let edited = ParagraphKey {
            generation: 14,
            ..key
        };
        text.shape(BlockKey(3), &style, &[edited], &src);
        assert_eq!(src.calls.get(), 2, "a new generation must fetch");
    }

    #[test]
    fn width_change_reflows_without_reshaping() {
        let Some((mut text, chain)) = latin_chain() else {
            return;
        };
        let key = ParagraphKey {
            namespace: 8,
            slot: 1,
            generation: 0,
        };
        let src = CountingSource {
            text: "alpha beta gamma delta",
            calls: std::cell::Cell::new(0),
        };
        let narrow = style_of(chain, Some(5.));
        let wide = style_of(chain, Some(100.));
        let narrow_handle = text.shape(BlockKey(1), &narrow, &[key], &src).unwrap();
        let narrow_lines = text.measure(narrow_handle).lines.len();
        let glyphs = text.shaped[&(ParagraphIdentity::Named(key), chain)]
            .run
            .glyphs
            .as_ptr();
        assert!(narrow_lines > 1);

        #[cfg(feature = "perf-counters")]
        crate::profiling::reset_work_counters();
        let wide_handle = text.shape(BlockKey(1), &wide, &[key], &src).unwrap();
        assert_eq!(text.measure(wide_handle).lines.len(), 1);
        assert_eq!(
            text.shaped[&(ParagraphIdentity::Named(key), chain)]
                .run
                .glyphs
                .as_ptr(),
            glyphs
        );
        assert_eq!(
            src.calls.get(),
            2,
            "new width still needs the source for flow"
        );
        #[cfg(feature = "perf-counters")]
        {
            let work = crate::profiling::work_counters();
            assert_eq!(work.shape_calls, 0);
            assert_eq!(work.flow_calls, 1);
        }

        let edited = ParagraphKey {
            generation: 1,
            ..key
        };
        text.shape(BlockKey(1), &wide, &[edited], &src).unwrap();
        assert_eq!(text.shaped.len(), 2, "a new generation needs new glyphs");
    }

    /// Restored from `engine.rs`. `is_single_glyph` is what a grid consumer uses
    /// to decide between drawing a sequence and drawing tofu.
    #[test]
    fn single_glyph_detection() {
        let Some((text, chain)) = latin_chain() else {
            return;
        };
        let d = text.diagnostics();
        assert!(d.is_single_glyph(chain, "A"), "one letter is one glyph");
        assert!(
            !d.is_single_glyph(chain, "AB"),
            "two letters shape to two glyphs"
        );
    }

    fn visible_count(text: &TextService, h: ShapedHandle, clip: Option<Rect>) -> usize {
        let mut n = 0;
        for_each_visible_glyph(
            text.measure(h),
            Vec2::new(0.0, 0.0),
            16.0,
            clip,
            |_, _, _, _| n += 1,
        );
        n
    }

    /// The three clip tests `engine.rs` had, restored against the extracted cull.
    /// `clip` culls on the CPU before anything is emitted — a scrolled body must
    /// not emit every paragraph's quads and lean on the GPU to discard them.
    ///
    /// **What these do and do not guard**, checked by mutation rather than assumed:
    /// deleting the per-glyph `glyph_intersects` test fails
    /// `clip_culls_glyphs_outside_it_horizontally`. Deleting the *line-level* cull
    /// fails nothing — the per-glyph test produces identical output without it, so
    /// the line cull is a pure performance fast path with no observable effect.
    /// Nothing here can guard it; only a benchmark can. Recorded so the next reader
    /// does not infer coverage that is not here.
    #[test]
    fn clip_culls_lines_outside_it() {
        let Some((mut text, chain)) = latin_chain() else {
            return;
        };
        let style = style_of(chain, None);
        let Some(h) = text.shape_transient(
            "alpha bravo\ncharlie delta\necho foxtrot\ngolf hotel",
            &style,
        ) else {
            return;
        };
        let all = visible_count(&text, h, None);
        assert!(all > 0, "unclipped must emit something");

        let first = text.measure(h).line(0).expect("a first line");
        let one_line = Rect::new(0.0, 0.0, 10_000.0, first.height_em * 16.0 * 0.9);
        let clipped = visible_count(&text, h, Some(one_line));
        assert!(clipped > 0, "the first line is inside the clip");
        assert!(
            clipped < all,
            "later lines must be culled, got {clipped} of {all}"
        );
    }

    #[test]
    fn clip_culls_glyphs_outside_it_horizontally() {
        let Some((mut text, chain)) = latin_chain() else {
            return;
        };
        let style = style_of(chain, None);
        let Some(h) = text.shape_transient("the quick brown fox jumps over it", &style) else {
            return;
        };
        let all = visible_count(&text, h, None);
        let narrow = Rect::new(0.0, 0.0, 20.0, 10_000.0);
        let clipped = visible_count(&text, h, Some(narrow));
        assert!(clipped < all, "a narrow clip must drop trailing glyphs");
    }

    #[test]
    fn a_clip_outside_the_block_emits_nothing() {
        let Some((mut text, chain)) = latin_chain() else {
            return;
        };
        let style = style_of(chain, None);
        let Some(h) = text.shape_transient("alpha bravo charlie", &style) else {
            return;
        };
        let far_away = Rect::new(50_000.0, 50_000.0, 100.0, 100.0);
        assert_eq!(visible_count(&text, h, Some(far_away)), 0);
    }

    /// The family a diagnostic reports must be the family *shaping* actually
    /// uses. Asserted against the shaper rather than a hardcoded font name, so
    /// the invariant holds whatever chain the box happens to have installed.
    ///
    /// Regression: `family_for` was "first face in the chain holding a glyph",
    /// which ignores emoji presentation entirely. With a colour font early in the
    /// chain that reported the colour face for every text-presentation character
    /// it happened to cover - 220 code points across planes 0-2, and because
    /// `glyph_bbox` took the same shortcut it moved geometry, not just labels.
    #[test]
    fn diagnostics_resolve_the_face_shaping_uses() {
        let Some((text, chain)) = emoji_first_chain() else {
            return;
        };
        let mut buf = [0u8; 4];
        for c in [
            '\u{2600}',
            '\u{263A}',
            '\u{2640}',
            '\u{2328}', // text presentation
            '\u{1F600}',
            '\u{1F308}', // emoji presentation
            'A',
            '1',
            '\u{00E9}', // plain text
        ] {
            let s = c.encode_utf8(&mut buf);
            let view = chain_view(&text.fonts, &text.chains, chain);
            let mut cache = GlyphCache::new();
            let run = shape_text(&view, &mut cache, s);
            let Some(glyph) = run.glyphs.first() else {
                continue;
            };
            if glyph.glyph_id == 0 {
                continue; // tofu; nothing to agree about
            }
            let shaped_family = text.fonts[glyph.font_id as usize].family_name();
            assert_eq!(
                text.diagnostics().family_for(chain, c),
                shaped_family,
                "U+{:04X}: the diagnostic disagrees with the face shaping picked",
                c as u32,
            );
        }
    }

    /// `glyph_bbox` reports the outline of the face that will actually be used,
    /// and reports nothing for a glyph that resolves to colour. It feeds cell
    /// fit-scaling in grid layouts, so resolving through a different face than
    /// shaping silently moves glyphs.
    #[test]
    fn glyph_bbox_follows_the_resolved_face() {
        let Some((text, chain)) = emoji_first_chain() else {
            return;
        };
        let mut buf = [0u8; 4];
        for c in ['\u{2600}', '\u{1F600}', 'A'] {
            let s = c.encode_utf8(&mut buf);
            let view = chain_view(&text.fonts, &text.chains, chain);
            let mut cache = GlyphCache::new();
            let run = shape_text(&view, &mut cache, s);
            let Some(glyph) = run.glyphs.first() else {
                continue;
            };
            if glyph.glyph_id == 0 {
                continue;
            }
            let bbox = text.diagnostics().glyph_bbox(chain, c);
            if glyph.is_color {
                assert!(
                    bbox.is_none(),
                    "U+{:04X} resolves to colour: no outline",
                    c as u32
                );
            } else {
                assert!(
                    bbox.is_some(),
                    "U+{:04X} resolves to an outline face",
                    c as u32
                );
            }
        }
    }

    struct StyledSource<'a> {
        text: &'a [&'a str],
        fonts: Vec<Vec<FontSpan>>,
    }
    impl ParagraphSource for StyledSource<'_> {
        fn paragraph_text(&self, i: usize, _: ParagraphKey) -> Option<Cow<'_, str>> {
            self.text.get(i).map(|s| Cow::Borrowed(*s))
        }
        fn paragraph_fonts(&self, i: usize, _: ParagraphKey) -> Cow<'_, [FontSpan]> {
            Cow::Borrowed(&self.fonts[i])
        }
    }
    fn span_keys(n: usize) -> Vec<ParagraphKey> {
        (0..n)
            .map(|i| ParagraphKey {
                namespace: 71,
                slot: i as u32,
                generation: 0,
            })
            .collect()
    }
    fn required_latin() -> (TextService, FontChainHandle) {
        latin_chain().expect("font-backed tests require DejaVu Sans, Segoe UI, or Arial")
    }
    fn bake(text: &mut TextService, draw: Draw) -> Vec<TextVertex> {
        let index = text.block_index(draw.block).unwrap();
        let key = GeomKey {
            color: draw.color.0.map(f32::to_bits),
            paint: draw.paint,
            clip: normalized_clip(&draw).map(|c| [c.x, c.y, c.width, c.height].map(f32::to_bits)),
        };
        text.rebuild_geometry(index, &draw, key);
        text.geometry[index].as_ref().unwrap().text.clone()
    }
    #[test]
    fn default_draw_is_unallocated_even_when_slot_zero_is_live() {
        let (mut text, chain) = required_latin();
        let live = text
            .shape_transient("live", &style_of(chain, None))
            .unwrap();
        let d = Draw::default();
        assert_ne!(live, d.block);
        assert_eq!(d.block, ShapedHandle::INVALID);
        assert!(text.block_index(d.block).is_none());
        assert_eq!(text.measure(d.block).len_bytes(), 0);
        assert_eq!(d.size, 1.);
        assert_eq!(d.color, Color([0., 0., 0., 1.]));
        assert!(d.paint.is_none() && d.clip.is_none());
        assert_eq!(
            std::mem::size_of::<Option<PaintHandle>>(),
            std::mem::size_of::<PaintHandle>()
        );
    }
    #[test]
    fn font_ranges_reject_split_graphemes_and_bad_order() {
        let (mut text, chain) = required_latin();
        let st = style_of(chain, None);
        for range in [0..1, 1..3, 0..4, 0..0] {
            let src = StyledSource {
                text: &["e\u{301}"],
                fonts: vec![vec![FontSpan { range, chain }]],
            };
            assert!(text.shape(BlockKey(70), &st, &span_keys(1), &src).is_none());
        }
        let src = StyledSource {
            text: &["e\u{301}"],
            fonts: vec![vec![FontSpan { range: 0..3, chain }]],
        };
        assert!(text.shape(BlockKey(70), &st, &span_keys(1), &src).is_some());
        let bad = vec![
            FontSpan { range: 2..3, chain },
            FontSpan { range: 0..1, chain },
        ];
        assert!(!valid_font_spans("abc", &bad));
        assert!(!valid_font_spans("👩‍💻", &[FontSpan { range: 0..4, chain }]));
    }
    #[test]
    fn equivalent_inline_faces_coalesce_and_chain_drop_is_dependency_local() {
        let (mut text, chain) = required_latin();
        let st = style_of(chain, Some(4.));
        let alias_fonts = text.chain_fonts(chain).unwrap().to_vec();
        let alias = text.register_chain(&alias_fonts).expect("font chain capacity");
        let plain = text
            .shape(
                BlockKey(70),
                &st,
                &span_keys(1),
                &Paragraphs(&["office cafe"]),
            )
            .unwrap();
        let src = StyledSource {
            text: &["office cafe"],
            fonts: vec![vec![
                FontSpan { range: 0..3, chain },
                FontSpan {
                    range: 3..11,
                    chain: alias,
                },
            ]],
        };
        let keys = vec![ParagraphKey {
            namespace: 72,
            ..span_keys(1)[0]
        }];
        #[cfg(feature = "perf-counters")]
        crate::profiling::reset_work_counters();
        let styled = text.shape(BlockKey(71), &st, &keys, &src).unwrap();
        #[cfg(feature = "perf-counters")]
        assert_eq!(crate::profiling::work_counters().shape_runs, 1);
        let signature = |layout: &Layout| {
            layout
                .lines
                .iter()
                .map(|l| {
                    (
                        l.metrics.width_em.to_bits(),
                        l.glyphs
                            .iter()
                            .map(|g| (g.font_id, g.glyph_id, g.cluster, g.x.to_bits()))
                            .collect::<Vec<_>>(),
                        l.carets
                            .iter()
                            .map(|c| (c.byte_index, c.x_em.to_bits()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            signature(text.measure(plain)),
            signature(text.measure(styled))
        );
        bake(
            &mut text,
            Draw {
                block: styled,
                ..Default::default()
            },
        );
        // The block must remember dependencies even after paragraph eviction.
        text.paragraphs.clear();
        text.drop_chain(alias);
        assert!(
            !text
                .shaped
                .contains_key(&(ParagraphIdentity::Named(keys[0]), chain))
        );
        assert!(text.block_index(styled).is_none());
        assert_eq!(text.measure(styled).len_bytes(), 0);
        assert!(text.geometry[styled.slot as usize].is_none());
        assert_eq!(text.measure(plain).len_bytes(), 11);
    }
    #[test]
    fn actual_italic_face_changes_glyphs_not_base_line_metrics() {
        let (mut text, chain) = required_latin();
        let data = font(&[
            "/usr/share/fonts/TTF/DejaVuSans-Oblique.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf",
            "C:/Windows/Fonts/segoeuii.ttf",
            "C:/Windows/Fonts/ariali.ttf",
            "/Library/Fonts/Arial Italic.ttf",
        ])
        .expect("font-span tests require an italic/oblique face");
        let italic = text.map_font(data, 0).unwrap();
        let ic = text.register_chain(&[italic]).expect("font chain capacity");
        let st = style_of(chain, None);
        let keys = span_keys(1);
        let plain = text
            .shape(BlockKey(70), &st, &keys, &Paragraphs(&["ab cd"]))
            .unwrap();
        let height = text.measure(plain).height_em();
        let src = StyledSource {
            text: &["ab cd"],
            fonts: vec![vec![FontSpan {
                range: 3..5,
                chain: ic,
            }]],
        };
        let keys = [ParagraphKey {
            generation: 1,
            ..keys[0]
        }];
        let h = text.shape(BlockKey(70), &st, &keys, &src).unwrap();
        assert_eq!(text.measure(h).height_em(), height);
        assert!(
            text.measure(h).lines[0]
                .glyphs
                .iter()
                .filter(|g| g.cluster >= 3)
                .all(|g| g.font_id == italic.0)
        );
        for byte in 0..=5 {
            assert_eq!(text.measure(h).caret_at(byte).byte_index, byte);
        }
        text.drop_chain(ic);
        assert!(text.shape(BlockKey(71), &st, &keys, &src).is_none());
    }
    #[test]
    fn paint_uses_block_bytes_and_recolors_without_shape_or_flow() {
        let (mut text, chain) = required_latin();
        let st = style_of(chain, None);
        let h = text
            .shape(BlockKey(70), &st, &span_keys(2), &Paragraphs(&["ab", "cd"]))
            .unwrap();
        let base = Color([0., 0., 0., 1.]);
        let red = Color([1., 0., 0., 1.]);
        let green = Color([0., 1., 0., 1.]);
        #[cfg(feature = "perf-counters")]
        crate::profiling::reset_work_counters();
        let p = text
            .register_paint(&[PaintSpan {
                range: 3..5,
                color: red,
            }])
            .unwrap();
        let draw = Draw {
            block: h,
            paint: Some(p),
            color: base,
            ..Default::default()
        };
        let a = bake(&mut text, draw);
        assert_eq!(a.len(), 24);
        assert!(a[..12].iter().all(|v| v.col == base.0));
        assert!(a[12..].iter().all(|v| v.col == red.0));
        let q = text
            .register_paint(&[PaintSpan {
                range: 3..5,
                color: green,
            }])
            .unwrap();
        let b = bake(
            &mut text,
            Draw {
                paint: Some(q),
                ..draw
            },
        );
        assert!(b[12..].iter().all(|v| v.col == green.0));
        assert_eq!(
            a.iter().map(|v| v.pos).collect::<Vec<_>>(),
            b.iter().map(|v| v.pos).collect::<Vec<_>>()
        );
        #[cfg(feature = "perf-counters")]
        {
            let c = crate::profiling::work_counters();
            assert_eq!(
                (c.shape_calls, c.flow_calls, c.source_reads, c.glyph_inserts),
                (0, 0, 0, 0)
            );
        }
        text.drop_paint(p);
        assert!(text.paints.get(p).is_none());
        assert!(
            a[12..].iter().all(|v| v.col == red.0),
            "baked pixels do not borrow paint"
        );
    }
    #[test]
    fn paint_colors_cluster_start_without_splitting_ligatures() {
        let (mut text, chain) = required_latin();
        let h = text
            .shape_transient("office", &style_of(chain, None))
            .unwrap();
        let clusters = text.measure(h).lines[0]
            .glyphs
            .iter()
            .filter(|g| g.info.is_some())
            .map(|g| g.cluster)
            .collect::<Vec<_>>();
        let red = Color([1., 0., 0., 1.]);
        let base = Color([0., 0., 0., 1.]);
        let p = text
            .register_paint(&[PaintSpan {
                range: 2..4,
                color: red,
            }])
            .unwrap();
        let verts = bake(
            &mut text,
            Draw {
                block: h,
                color: base,
                paint: Some(p),
                ..Default::default()
            },
        );
        assert_eq!(verts.len(), clusters.len() * 6);
        for (quad, cluster) in verts.chunks_exact(6).zip(clusters) {
            assert!(quad.iter().all(|v| v.col
                == if (2..4).contains(&cluster) {
                    red.0
                } else {
                    base.0
                }));
        }
    }
}
