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
/// [`Boundaries`]. `PageUp`/`PageDown` carry their stride in visual lines,
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
pub trait Boundaries {
    fn prev_word(&self, byte_index: usize) -> Option<usize>;
    fn next_word(&self, byte_index: usize) -> Option<usize>;
}

impl Boundaries for () {
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
    pub fn hit_test(&self, at_em: Vec2) -> Option<Caret> {
        let line_index = self.line_index_for_y(at_em.y)?;
        let byte_index = self.caret_on_line(line_index, at_em.x)?;
        Some(Caret {
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
            .or_else(|| self.line_for_byte(byte_index))
            .unwrap_or(0);
        Caret { byte_index, line_index }
    }

    /// The caret for a byte, with the layout's default (start-affine) line.
    ///
    /// One correction over raw [`Layout::line_for_byte`]: a hard break's end
    /// byte belongs to no line, and `line_for_byte` answers with the following
    /// line — whose start is *past* the byte. That byte pins to the line whose
    /// end it is, which is also what puts a blank line's caret on the blank
    /// line rather than the one below it.
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
        Caret { byte_index, line_index }
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
    /// beside the caret. `text` supplies word boundaries ([`Boundaries`]); pass
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
        text: &impl Boundaries,
    ) -> Caret {
        if self.lines.is_empty() {
            *goal = None;
            return Caret { byte_index: 0, line_index: 0 };
        }
        let caret = self.clamp_caret(caret);

        // Horizontal placement: boundary bytes keep the line the caret came
        // from; elsewhere `caret_at`'s default resolution.
        let place = |byte_index: usize| -> Caret {
            let byte_index = byte_index.min(self.len_bytes());
            if let Some(range) = self.line_range(caret.line_index) {
                if byte_index == range.start || byte_index == range.end {
                    return Caret { byte_index, line_index: caret.line_index };
                }
            }
            self.caret_at(byte_index)
        };

        let vertical = |goal: &mut Option<f32>, delta: isize| -> Caret {
            let current = caret.line_index;
            let target = (current as isize + delta)
                .clamp(0, self.lines.len() as isize - 1) as usize;
            if target == current {
                // Boundary line: Up snaps to its start, Down to its end.
                let range = self.line_range(current);
                let byte_index = if delta < 0 {
                    range.map(|r| r.start).unwrap_or(0)
                } else {
                    range.map(|r| r.end).unwrap_or_else(|| self.len_bytes())
                };
                return Caret { byte_index, line_index: current };
            }
            let x = *goal.get_or_insert_with(|| {
                self.caret_rect_on_line(Some(current), caret.byte_index).x_em
            });
            let byte_index = self.caret_on_line(target, x).unwrap_or(caret.byte_index);
            Caret { byte_index, line_index: target }
        };

        match motion {
            Motion::Left => {
                *goal = None;
                place(self.prev_caret_stop(caret.byte_index).unwrap_or(caret.byte_index))
            }
            Motion::Right => {
                *goal = None;
                place(self.next_caret_stop(caret.byte_index).unwrap_or(caret.byte_index))
            }
            Motion::WordLeft => {
                *goal = None;
                place(text.prev_word(caret.byte_index).unwrap_or_else(|| {
                    self.prev_caret_stop(caret.byte_index).unwrap_or(caret.byte_index)
                }))
            }
            Motion::WordRight => {
                *goal = None;
                place(text.next_word(caret.byte_index).unwrap_or_else(|| {
                    self.next_caret_stop(caret.byte_index).unwrap_or(caret.byte_index)
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
            Motion::Up => vertical(goal, -1),
            Motion::Down => vertical(goal, 1),
            Motion::PageUp(lines) => vertical(goal, -(lines as isize)),
            Motion::PageDown(lines) => vertical(goal, lines as isize),
            Motion::DocStart => {
                *goal = None;
                Caret { byte_index: 0, line_index: 0 }
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
    /// by the caller's [`Boundaries`] (words are semantics, not shaping); a
    /// declining classifier degrades to the cluster around the byte.
    pub fn select_word_at(&self, byte_index: usize, text: &impl Boundaries) -> Range<usize> {
        let byte_index = byte_index.min(self.len_bytes());
        let start = text.prev_word(byte_index).unwrap_or_else(|| {
            self.prev_caret_stop(byte_index).unwrap_or(byte_index)
        });
        let end = text.next_word(byte_index).unwrap_or_else(|| {
            self.next_caret_stop(byte_index).unwrap_or(byte_index)
        });
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
            let newline_selected = hard_break
                && range.start <= line.byte_range.end
                && range.end > line.byte_range.end;
            if start >= end && !newline_selected {
                continue;
            }
            let x0 = caret_x_on(line, start.min(end));
            let x1 = caret_x_on(line, end.max(start));
            let stub = if newline_selected { NEWLINE_STUB_EM } else { 0.0 };
            spans.push(SelectionSpan {
                line: index,
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
    /// Bumped on every (re)assembly. An edit reshapes a block **in place** —
    /// same key, same slot, same handle — so a retained [`Batch`] cannot see
    /// it in anything it holds; this is what `batch_live` checks instead.
    revision: u64,
}

struct Block {
    key: BlockKey,
    style: Style,
    parts: Vec<ParagraphKey>,
    layout: Layout,
    last_used: u64,
}

/// What the cached vertices were built for. Color is baked per-vertex — the
/// price of letting differently-colored blocks share one draw call — so it is
/// part of the key. `at` and `size` are deliberately absent: quads bake at the
/// origin and unit size, and `place` applies both on the way out.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GeomKey {
    color: [u32; 4],
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
    /// Emoji atlas eviction epoch the UVs were baked against. A change means a
    /// cell was recycled and the cached quads may now point at another glyph —
    /// the invalidation the old public `emoji_epoch` made the *consumer* do.
    epoch: u64,
    /// Raster bucket the color glyphs were baked against. Held here rather than
    /// in [`GeomKey`] for the same reason as `epoch`: it is the one thing a size
    /// change still decides, and it decides nothing for a block with no color
    /// glyphs. Keying on it would rebuild every text block in the frame each time
    /// a zoom crossed a bucket boundary, for nothing.
    bucket: u32,
    text: Vec<TextVertex>,
    emoji: Vec<EmojiVertex>,
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
    /// Byte and vertex bounds into the batch buffer, per pipeline. Private:
    /// the buffer layout and the text/emoji stride split are not surface.
    text_bytes: (u64, u64),
    text_vertices: (u32, u32),
    emoji_bytes: (u64, u64),
    emoji_vertices: (u32, u32),
}

/// Blocks the consumer chose to group, concatenated into one GPU buffer **the
/// consumer holds** — the unit of vertex ownership (see the "`Batch` owns its
/// vertices" lock in `decisions.md`).
///
/// Nothing here is shared, so nothing can be clobbered: the buffer lives
/// exactly as long as the `Batch`, and dropping it immediately after recording
/// is fine — wgpu ref-counts what a pass binds. Rebuilding is the consumer's
/// call (content changed, [`TextService::batch_live`] went false, a recolor —
/// color is baked per-vertex); an unchanged batch costs zero per-frame upload.
///
/// Order within a batch is the order of the `&[Draw]` it was prepared from —
/// that is z-order, and it is the consumer's: `prepare` never reorders, and
/// drawing segments out of order is a silent z bug the service cannot detect.
pub struct Batch {
    buffer: wgpu::Buffer,
    segments: Vec<Segment>,
    /// Emoji-atlas eviction epoch the UVs were baked against; `None` for a
    /// batch with no emoji.
    emoji_epoch: Option<u64>,
    /// The blocks these vertices were baked from, as `(slot, generation,
    /// revision)`. An edit reshapes a block in place — same key, slot and
    /// handle — so nothing in the consumer's draw list changes; the revision
    /// is how `batch_live` sees it. Adjacent duplicates are collapsed.
    blocks: Vec<(u32, u32, u64)>,
}

impl Batch {
    /// Where the clip changes: one entry per scissor-uniform run, in draw
    /// order. A uniform-clip batch has exactly one.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }
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
/// device except [`TextService::draw`], which takes one as a parameter. That is what
/// lets model code hold a `&TextService` for measurement and hit-testing without
/// reaching for the GPU, and what makes shaping work headless.
#[derive(Default)]
pub struct TextService {
    fonts: Vec<Font>,
    /// `None` for a dropped slot, so live handles stay valid across a reload.
    chains: Vec<Option<Vec<FontHandle>>>,

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
    block_lookup: HashMap<BlockKey, ShapedHandle>,
    paragraphs: HashMap<(ParagraphKey, Style), ParaLayout>,

    glyphs: GlyphCache,
    emoji: EmojiCache,
    gpu: Option<Gpu>,
    pending_format: Option<wgpu::TextureFormat>,
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
                    self.blocks.push(BlockSlot { block: None, revision: 0 });
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

    /// One-paragraph convenience over [`TextService::shape`] — a title, a field.
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
    /// once per pass. Screen-space is [`TextService::pixel_ortho`]; world or 3D text is
    /// an MVP — same call, no mode flag.
    pub fn set_transform(&mut self, queue: &wgpu::Queue, transform: [f32; 16]) {
        if let Some(gpu) = &self.gpu {
            let matrix = glam::Mat4::from_cols_array(&transform);
            gpu.text.write_matrix(queue, matrix);
            gpu.emoji.write_matrix(queue, matrix);
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
    /// magnification. Zero and negatives are ignored.
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

    /// Record glyph quads for `h` into `pass`.
    ///
    /// `at` and `size` are in the transform's source space — screen pixels under
    /// [`TextService::pixel_ortho`], world units under an MVP. `at` is the **top-left**
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

    /// Build a [`Batch`] the consumer owns: every block's quads (cached, and
    /// reused across frames while its draw parameters hold) concatenated into
    /// one buffer, split into [`Segment`]s where the clip changes.
    ///
    /// **All mutation happens here** — geometry rebuilds, emoji rasterization,
    /// atlas sync, one buffer upload. `draw_segment`/`draw_prepared` purely
    /// record, so "never recreate a texture between a bind and its draw" holds
    /// by construction rather than by call-order discipline.
    ///
    /// Input order is preserved (it is z-order, and it is yours); adjacent
    /// items with an equal clip coalesce into one segment. Retention is only
    /// meaningful if `at`/`size` are stable across frames — under a camera,
    /// pass world units with the camera in the transform (`set_transform`),
    /// not pre-projected pixels, or every pan invalidates every batch.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        items: &[Draw],
    ) -> Batch {
        self.ensure_gpu(device);
        if self.gpu.is_none() {
            // No target format was ever set, so there is no pipeline to bind
            // and nothing to bind it to. A `Batch` owns a buffer by
            // definition; this one is never bound, having no segments.
            return Batch {
                buffer: batch_buffer(device, 0),
                segments: Vec::new(),
                emoji_epoch: None,
                blocks: Vec::new(),
            };
        }

        // Read *before* the walk: rasterizing a new emoji can evict, and the
        // quads placed up to that point were baked against this value. It is
        // also what `rebuild_geometry` stamps, so the batch and the geometry
        // pool agree about what they were built against.
        let epoch = self.emoji.epoch();
        let pixel_scale = self.pixel_scale.unwrap_or(1.0);
        let mut text_verts = std::mem::take(&mut self.batch_text);
        let mut emoji_verts = std::mem::take(&mut self.batch_emoji);
        text_verts.clear();
        emoji_verts.clear();
        // One growth up front. A dense frame concatenates tens of thousands of
        // small runs, and letting the Vec double its way there is most of the
        // batch cost.
        text_verts.reserve(items.len() * 6);

        let mut segments: Vec<Segment> = Vec::new();
        let mut blocks: Vec<(u32, u32, u64)> = Vec::new();
        for run in clip_runs(items) {
            let text_start = text_verts.len() as u32;
            let emoji_start = emoji_verts.len() as u32;
            for item in &items[run.clone()] {
                let Some(index) = self.block_index(item.block) else {
                    continue;
                };
                let baked = (item.block.slot, item.block.generation, self.blocks[index].revision);
                if blocks.last() != Some(&baked) {
                    blocks.push(baked);
                }
                // The bucket wants an *on-screen* size: under an MVP `size` is
                // world units, and the consumer-stated scale bridges the gap.
                let bucket = bucket_for(item.size * pixel_scale);
                let key = GeomKey {
                    color: item.color.0.map(f32::to_bits),
                    clip: normalized_clip(item)
                        .map(|c| [c.x, c.y, c.width, c.height].map(f32::to_bits)),
                };
                // Fast path: one dense-array probe, then straight into the batch.
                if let Some(geom) = self.geometry[index].as_ref() {
                    // The epoch only matters if this block actually baked emoji
                    // UVs; an atlas eviction cannot disturb a text-only block,
                    // and invalidating those too rebuilds the frame for nothing.
                    let emoji_still_valid =
                        geom.emoji.is_empty() || (geom.epoch == epoch && geom.bucket == bucket);
                    if geom.key == key && emoji_still_valid {
                        place(&mut text_verts, &mut emoji_verts, geom, item.at, item.size);
                        continue;
                    }
                }
                self.rebuild_geometry(index, item, key, epoch, bucket);
                if let Some(geom) = self.geometry[index].as_ref() {
                    place(&mut text_verts, &mut emoji_verts, geom, item.at, item.size);
                }
            }
            segments.push(Segment {
                clip: items[run.start].clip,
                // Byte spans are batch-global under this layout and are only
                // known once both halves are sized; filled in below.
                text_bytes: (0, 0),
                text_vertices: (text_start, text_verts.len() as u32),
                emoji_bytes: (0, 0),
                emoji_vertices: (emoji_start, emoji_verts.len() as u32),
            });
        }

        // Atlas uploads are `queue` writes, which land before this batch is
        // ever drawn, so *adding* glyphs here is safe. The invariants that keep
        // it safe — never recreate a texture already bound by an earlier draw in
        // the pass, never evict a slot that draw sampled — live with the atlases.
        let gpu = self.gpu.as_mut().expect("checked above");
        gpu.text_atlas
            .sync(device, queue, &gpu.text.atlas_layout, &self.glyphs);
        gpu.emoji_atlas
            .sync(device, queue, &gpu.emoji.atlas_layout, &self.emoji);

        // One buffer per batch, text quads then emoji quads: two writes and two
        // `set_vertex_buffer` offsets whatever the segment count. Both strides
        // (56 and 16) are multiples of four, so the emoji half starts at a legal
        // offset however much text precedes it.
        let text_data: &[u8] = bytemuck::cast_slice(&text_verts);
        let emoji_data: &[u8] = bytemuck::cast_slice(&emoji_verts);
        let split = text_data.len() as u64;
        let total = split + emoji_data.len() as u64;
        let buffer = batch_buffer(device, total);
        if !text_data.is_empty() {
            queue.write_buffer(&buffer, 0, text_data);
        }
        if !emoji_data.is_empty() {
            queue.write_buffer(&buffer, split, emoji_data);
        }
        for segment in &mut segments {
            segment.text_bytes = (0, split);
            segment.emoji_bytes = (split, total);
        }

        let emoji_epoch = (!emoji_verts.is_empty()).then_some(epoch);
        self.batch_text = text_verts;
        self.batch_emoji = emoji_verts;
        Batch {
            buffer,
            segments,
            emoji_epoch,
            blocks,
        }
    }

    /// Record one segment of a prepared batch. Sets **no scissor** — set yours
    /// first from [`Segment::clip`]. Within a segment, text draws below emoji.
    ///
    /// Recording only: `&self`, no device, nothing is uploaded or rebuilt. A
    /// no-op if `index` is out of range or no target was ever set.
    pub fn draw_segment(&self, pass: &mut wgpu::RenderPass<'_>, batch: &Batch, index: usize) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let Some(seg) = batch.segments.get(index) else {
            return;
        };
        gpu.text.draw_vertices(
            pass,
            &gpu.text_atlas,
            &batch.buffer,
            seg.text_bytes.0..seg.text_bytes.1,
            seg.text_vertices.0..seg.text_vertices.1,
        );
        gpu.emoji.draw(
            pass,
            &gpu.emoji_atlas,
            &batch.buffer,
            seg.emoji_bytes.0..seg.emoji_bytes.1,
            seg.emoji_vertices.0..seg.emoji_vertices.1,
        );
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

    /// Whether a retained batch still draws what it was baked from. Integer
    /// compares only — check per frame, re-`prepare` on `false`. Goes stale
    /// when the emoji atlas evicted under its UVs, **or when any block it
    /// baked has since reshaped or been evicted**. The second clause is the
    /// one a consumer cannot answer for itself: an edit reshapes a block *in
    /// place* — same key, same slot, same handle — so the consumer's draw
    /// list compares identical while the vertices it retained are of the old
    /// text. The service did the reshape; the service answers for it.
    /// Drawing a stale batch is defined: old vertices and texels — stale
    /// pixels, never UB.
    pub fn batch_live(&self, batch: &Batch) -> bool {
        epoch_live(batch.emoji_epoch, self.emoji.epoch())
            && batch.blocks.iter().all(|&(slot, generation, revision)| {
                self.generations.get(slot as usize) == Some(&generation)
                    && self
                        .blocks
                        .get(slot as usize)
                        .is_some_and(|b| b.revision == revision)
            })
    }

    /// Record many blocks in **one** pair of draw calls: `prepare` +
    /// `draw_prepared` + drop. The easy path *is* the hard path plus a drop —
    /// there is no second route to the GPU.
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
    /// no upload; `prepare` does that once for the whole batch.
    fn rebuild_geometry(
        &mut self,
        index: usize,
        item: &Draw,
        key: GeomKey,
        epoch: u64,
        bucket: u32,
    ) {
        let color = item.color;
        // Bake at the origin *and* at unit size, with the clip normalised to
        // match, so the build depends on everything about this draw except where
        // it lands and how big it is. `place` applies both on the way out.
        let at = Vec2::new(0.0, 0.0);
        let size = 1.0f32;
        let clip = normalized_clip(item);
        let mut text_verts = Vec::new();
        let mut emoji_verts = Vec::new();

        // Emoji rasterization mutates the emoji cache, so it cannot happen while
        // the block is borrowed; collect the requests, then service them.
        let mut emoji_requests: Vec<(u16, u32, f32, f32)> = Vec::new();
        {
            let block = self.blocks[index].block.as_ref().expect("checked by caller");
            for_each_visible_glyph(&block.layout, at, size, clip, |glyph, pen_x, pen_y| {
                if glyph.is_color {
                    emoji_requests.push((glyph.font_id, glyph.glyph_id, pen_x, pen_y));
                } else if let Some(info) = glyph.info {
                    push_glyph_quad_pixels(&mut text_verts, &info, pen_x, pen_y, size, color.0);
                }
            });
        }

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
            bucket,
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
    mut f: impl FnMut(&ShapedGlyph, f32, f32),
) {
    for line in &layout.lines {
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
                // A colour glyph is a size x size box hanging from the baseline;
                // it has no outline to measure, so the box is the bound.
                let outside = clip.is_some_and(|clip| {
                    pen_y < clip.y
                        || pen_y - size > clip.max_y()
                        || pen_x + size < clip.x
                        || pen_x > clip.max_x()
                });
                if !outside {
                    f(glyph, pen_x, pen_y);
                }
                continue;
            }
            let Some(info) = glyph.info else { continue };
            if clip.is_some_and(|clip| !glyph_intersects(&info, pen_x, pen_y, size, clip)) {
                continue;
            }
            f(glyph, pen_x, pen_y);
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
    ///
    /// Routed through the same `layout::face_for_grapheme` the shaper uses, so
    /// this cannot disagree with what gets drawn. Resolving
    /// it here independently — "first face with a glyph" — silently ignores
    /// emoji presentation and reports the color font for every text-presentation
    /// character it happens to cover.
    pub fn family_for(&self, chain: FontChainHandle, c: char) -> Option<String> {
        let view = self.text.chain_view(chain);
        let mut buf = [0u8; 4];
        let entry = view.get(crate::layout::face_for_grapheme(&view, c.encode_utf8(&mut buf)))?;
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
        let entry = view.get(crate::layout::face_for_grapheme(&view, c.encode_utf8(&mut buf)))?;
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
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("sanscale batch vertices"),
        // A batch that placed nothing still owns a buffer; keep it nominally
        // sized rather than relying on zero-length buffers being legal.
        size: size.max(4),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The staleness rule behind [`TextService::batch_live`], without a device:
/// a batch that baked no emoji UVs has nothing an eviction can repoint.
fn epoch_live(baked: Option<u64>, current: u64) -> bool {
    baked.is_none_or(|epoch| epoch == current)
}

/// Concatenate one block's origin-baked quads into the batch, translated to
/// `at`. The copy is a memcpy plus a linear add — orders of magnitude cheaper
/// than the rebuild that baking `at` into the cache key would have forced.
fn place(
    text: &mut Vec<TextVertex>,
    emoji: &mut Vec<EmojiVertex>,
    geom: &Geometry,
    at: Vec2,
    size: f32,
) {
    let base = text.len();
    text.extend_from_slice(&geom.text);
    for v in &mut text[base..] {
        v.pos[0] = v.pos[0] * size + at.x;
        v.pos[1] = v.pos[1] * size + at.y;
    }
    if !geom.emoji.is_empty() {
        let base = emoji.len();
        emoji.extend_from_slice(&geom.emoji);
        for v in &mut emoji[base..] {
            v.pos[0] = v.pos[0] * size + at.x;
            v.pos[1] = v.pos[1] * size + at.y;
        }
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

    /// Three hard-broken lines — "ab" (0..2), blank (3..3), "cd" (4..6) — with
    /// unit-advance caret stops, GPU- and font-free via `from_lines`.
    fn three_line_layout() -> Layout {
        let line = |bytes: Range<usize>, top: f32| LayoutLineSpec {
            carets: bytes
                .clone()
                .chain([bytes.end])
                .enumerate()
                .map(|(i, byte_index)| CaretStop { byte_index, x_em: i as f32 })
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

    /// A selected hard newline shows as a stub past the last glyph, and a blank
    /// line inside the selection is a stub rather than nothing at all.
    #[test]
    fn selection_shows_newline_stubs_and_blank_lines() {
        let layout = three_line_layout();
        let spans = layout.selection(1..5);
        assert_eq!(spans.len(), 3, "every line the selection touches has a span");
        assert!(spans[0].width_em > 1.0, "line 0: one glyph plus the newline stub");
        assert_eq!(spans[1].line, 1);
        assert!(spans[1].width_em > 0.0, "blank line: a visible stub, not nothing");
        assert!((spans[2].width_em - 1.0).abs() < 1e-6, "line 2: one glyph, newline not selected");
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
                .map(|(i, byte_index)| CaretStop { byte_index, x_em: i as f32 })
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
        let caret = layout.caret_move(Caret { byte_index: 5, line_index: 1 }, Motion::Left, &mut goal, &());
        assert_eq!((caret.byte_index, caret.line_index), (4, 1));

        // Up on the top line snaps to its start; Down on the bottom to its end.
        let caret = layout.caret_move(layout.caret_at(2), Motion::Up, &mut goal, &());
        assert_eq!((caret.byte_index, caret.line_index), (0, 0));
        let caret = layout.caret_move(Caret { byte_index: 5, line_index: 1 }, Motion::Down, &mut goal, &());
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
        impl Boundaries for Stub {
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
        let style = Style { chain, wrap_em: None, align: Align::Left, line_spacing: 1.0 };
        let key = |generation| ParagraphKey { namespace: 9, slot: 1, generation };
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
        let latin = font(&["C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/arial.ttf"])?;
        let mut text = TextService::new();
        let e = text.map_font(emoji, 0).ok()?;
        let l = text.map_font(latin, 0).ok()?;
        let chain = text.register_chain(&[e, l]);
        Some((text, chain))
    }

    fn latin_chain() -> Option<(TextService, FontChainHandle)> {
        let latin = font(&["C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/arial.ttf"])?;
        let mut text = TextService::new();
        let h = text.map_font(latin, 0).ok()?;
        let chain = text.register_chain(&[h]);
        Some((text, chain))
    }

    fn style_of(chain: FontChainHandle, wrap_em: Option<f32>) -> Style {
        Style { chain, wrap_em, align: Align::Left, line_spacing: 1.0 }
    }

    /// A `Draw` that differs from its neighbours only in `clip` — enough for
    /// the segmentation rule, which reads nothing else. No font, no device.
    fn clipped(clip: Option<Rect>) -> Draw {
        Draw {
            block: ShapedHandle { slot: 0, generation: 0 },
            at: Vec2::new(0.0, 0.0),
            size: 16.0,
            color: Color([1.0, 1.0, 1.0, 1.0]),
            clip,
        }
    }

    /// A segment is a scissor change, and a scissor that didn't change isn't
    /// one. Splitting per item instead would be correct and quietly cost a
    /// draw call per block — the thing batching exists to avoid.
    #[test]
    fn adjacent_equal_clips_coalesce() {
        let clip = Some(Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(clip_runs(&[clipped(clip), clipped(clip), clipped(clip)]), vec![0..3]);
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

    /// A text-only batch is immortal: the text atlas grows but never evicts or
    /// moves a slot, so there is nothing for an epoch to invalidate. Keying
    /// staleness on the epoch alone would re-`prepare` every retained batch in
    /// the document each time one emoji cell was recycled.
    #[test]
    fn a_batch_with_no_emoji_never_goes_stale() {
        assert!(epoch_live(None, 0));
        assert!(epoch_live(None, u64::MAX));
        assert!(epoch_live(Some(4), 4));
        assert!(!epoch_live(Some(4), 5), "an eviction since the bake is staleness");
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
        let key = ParagraphKey { namespace: 7, slot: 11, generation: 13 };
        let src = CountingSource {
            text: "cached paragraph should only ever be fetched once",
            calls: std::cell::Cell::new(0),
        };

        assert!(text.shape(BlockKey(1), &style, &[key], &src).is_some());
        assert_eq!(src.calls.get(), 1, "the first shape must fetch");

        // Same block, same parts: the block-level comparison short-circuits.
        text.shape(BlockKey(1), &style, &[key], &src);
        assert_eq!(src.calls.get(), 1, "re-shaping an unchanged block must not fetch");

        // A *different* block over the same paragraph: the block is new, so this
        // reaches the paragraph pool — which must still hit.
        text.shape(BlockKey(2), &style, &[key], &src);
        assert_eq!(src.calls.get(), 1, "a paragraph-cache hit must not fetch");

        // Bumping the generation is what invalidation looks like, and must fetch.
        let edited = ParagraphKey { generation: 14, ..key };
        text.shape(BlockKey(3), &style, &[edited], &src);
        assert_eq!(src.calls.get(), 2, "a new generation must fetch");
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
        assert!(!d.is_single_glyph(chain, "AB"), "two letters shape to two glyphs");
    }

    fn visible_count(text: &TextService, h: ShapedHandle, clip: Option<Rect>) -> usize {
        let mut n = 0;
        for_each_visible_glyph(text.measure(h), Vec2::new(0.0, 0.0), 16.0, clip, |_, _, _| n += 1);
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
        assert!(clipped < all, "later lines must be culled, got {clipped} of {all}");
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
            '\u{2600}', '\u{263A}', '\u{2640}', '\u{2328}', // text presentation
            '\u{1F600}', '\u{1F308}', // emoji presentation
            'A', '1', '\u{00E9}', // plain text
        ] {
            let s = c.encode_utf8(&mut buf);
            let view = chain_view(&text.fonts, &text.chains, chain);
            let mut cache = GlyphCache::new();
            let run = shape_text(&view, &mut cache, s);
            let Some(glyph) = run.glyphs.first() else { continue };
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
            let Some(glyph) = run.glyphs.first() else { continue };
            if glyph.glyph_id == 0 {
                continue;
            }
            let bbox = text.diagnostics().glyph_bbox(chain, c);
            if glyph.is_color {
                assert!(bbox.is_none(), "U+{:04X} resolves to colour: no outline", c as u32);
            } else {
                assert!(bbox.is_some(), "U+{:04X} resolves to an outline face", c as u32);
            }
        }
    }
}
