//! High-level text layout API: font + caches + per-frame glyph buffer.
//!
//! Vertices from [`TextEngine::flush`] use **window pixel** coordinates for `TextVertex::pos`.
//! Pass an orthographic projection that maps `(0,0)..(width,height)` to clip space (no extra
//! translation/scale for font size — each [`TextArgs::size_px`] is baked into layout and quads).

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::cache::{GlyphCache, GlyphInfo};
use crate::emoji::{bucket_for, EmojiCache, EmojiSlot};
use crate::font::{Font, FontMetrics, FontSet, FontSource};
use crate::layout::{shape_text, ShapedGlyph};
use crate::renderer::{EmojiAtlas, TextAtlas};
use crate::vertex::{push_emoji_quad, push_glyph_quad_pixels, EmojiVertex, TextVertex};

const CACHE_TTL_FRAMES: u64 = 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ParagraphIdentity {
    pub namespace: u64,
    pub slot: u32,
    pub generation: u32,
}

pub struct TextParagraph<'a> {
    pub text: &'a str,
    pub identity: Option<ParagraphIdentity>,
    pub byte_start: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TextParagraphIdentity {
    pub identity: ParagraphIdentity,
    pub byte_start: usize,
    pub byte_len: usize,
}

pub trait TextParagraphProvider {
    fn paragraph_text(&mut self, identity: ParagraphIdentity) -> Option<Cow<'_, str>>;
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum ParagraphKey {
    Content(Arc<str>),
    Identity(ParagraphIdentity),
}

impl ParagraphKey {
    fn content(content: &str) -> Self {
        Self::Content(Arc::from(content))
    }

    fn identity(identity: ParagraphIdentity) -> Self {
        Self::Identity(identity)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct ParagraphLayoutKey {
    paragraph: ParagraphKey,
    max_width_em_bits: u32,
}

impl ParagraphLayoutKey {
    fn new(paragraph: ParagraphKey, max_width_em: f32) -> Self {
        Self {
            paragraph,
            max_width_em_bits: max_width_em.to_bits(),
        }
    }
}

/// Arguments controlling sizing, wrapping, and alignment for `text` / `measure`.
#[derive(Clone)]
pub struct TextArgs {
    pub size_px: f32,
    pub color: [f32; 4],
    pub max_width_px: Option<f32>,
    /// Multiplier on the font metrics line height (not pixel height).
    pub line_spacing: f32,
    pub align: Align,
}

impl Default for TextArgs {
    fn default() -> Self {
        Self {
            size_px: 16.0,
            color: [0.0, 0.0, 0.0, 1.0],
            max_width_px: None,
            line_spacing: 1.2,
            align: Align::Left,
        }
    }
}

/// Pixel-space clip rectangle for text emission.
///
/// Coordinates live in the same space as [`PushedGlyph`] and flushed
/// [`TextVertex`] positions. Glyphs outside this rectangle are not pushed into
/// the current frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextClip {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl TextClip {
    pub fn from_min_size(x: f32, y: f32, width: f32, height: f32) -> Option<Self> {
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
        {
            return None;
        }
        Some(Self {
            min_x: x,
            min_y: y,
            max_x: x + width,
            max_y: y + height,
        })
    }

    fn intersects_y(self, top: f32, bottom: f32) -> bool {
        bottom >= self.min_y && top <= self.max_y
    }

    fn intersects_glyph(self, info: &GlyphInfo, pen_x: f32, pen_y: f32, size: f32) -> bool {
        let (min_x, min_y, max_x, max_y) = info.bbox;
        let left = pen_x + min_x * size;
        let right = pen_x + max_x * size;
        let top = pen_y - max_y * size;
        let bottom = pen_y - min_y * size;
        right >= self.min_x && left <= self.max_x && bottom >= self.min_y && top <= self.max_y
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}


#[derive(Clone, Debug)]
pub struct CaretStop {
    pub byte_index: usize,
    pub x_px: f32,
}

#[derive(Clone, Debug)]
pub struct TextLayoutLine {
    pub byte_range: Range<usize>,
    pub top_px: f32,
    pub baseline_px: f32,
    pub height_px: f32,
    pub width_px: f32,
    pub carets: Vec<CaretStop>,
}

#[derive(Clone, Debug, Default)]
pub struct TextLayout {
    pub lines: Vec<TextLayoutLine>,
    pub width_px: f32,
    pub height_px: f32,
    glyphs: Vec<ShapedGlyph>,
    line_breaks: Vec<u32>,
    line_advances: Vec<f32>,
    block_width_em: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct SelectionSpan {
    pub line: usize,
    pub x_px: f32,
    pub y_px: f32,
    pub width_px: f32,
    pub height_px: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct CaretRect {
    pub x_px: f32,
    pub y_px: f32,
    pub height_px: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct CaretHit {
    pub byte_index: usize,
    pub line_index: usize,
}

impl TextLayout {
    pub fn scale_pixels(&mut self, scale: f32) {
        if !scale.is_finite() || scale <= 0.0 || (scale - 1.0).abs() <= f32::EPSILON {
            return;
        }

        self.width_px *= scale;
        self.height_px *= scale;
        for line in &mut self.lines {
            line.top_px *= scale;
            line.baseline_px *= scale;
            line.height_px *= scale;
            line.width_px *= scale;
            for caret in &mut line.carets {
                caret.x_px *= scale;
            }
        }
    }

    pub fn hit_test(&self, x_px: f32, y_px: f32) -> usize {
        self.hit_test_position(x_px, y_px)
            .map(|hit| hit.byte_index)
            .unwrap_or(0)
    }

    pub fn hit_test_position(&self, x_px: f32, y_px: f32) -> Option<CaretHit> {
        let line_index = self.line_index_for_y(y_px)?;
        let line = self.lines.get(line_index)?;

        if x_px <= 0.0 {
            return Some(CaretHit {
                byte_index: line.byte_range.start,
                line_index,
            });
        }
        if x_px >= line.width_px {
            return Some(CaretHit {
                byte_index: line.byte_range.end,
                line_index,
            });
        }
        let byte_index = nearest_caret(line, x_px)
            .map(|caret| caret.byte_index)
            .unwrap_or(line.byte_range.start);
        Some(CaretHit {
            byte_index,
            line_index,
        })
    }

    pub fn line_index_for_byte(&self, byte_index: usize) -> Option<usize> {
        self.lines
            .iter()
            .enumerate()
            .rfind(|(_, line)| line.byte_range.start == byte_index)
            .map(|(index, _)| index)
            .or_else(|| {
                self.lines
                    .iter()
                    .enumerate()
                    .find(|(_, line)| {
                        byte_index > line.byte_range.start && byte_index <= line.byte_range.end
                    })
                    .map(|(index, _)| index)
            })
            .or_else(|| (!self.lines.is_empty()).then_some(self.lines.len() - 1))
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line_range_for_byte(&self, byte_index: usize) -> Option<Range<usize>> {
        self.line_index_for_byte(byte_index)
            .and_then(|index| self.lines.get(index))
            .map(|line| line.byte_range.clone())
    }

    pub fn nearest_caret_on_line(&self, line_index: usize, x_px: f32) -> Option<usize> {
        self.lines.get(line_index).and_then(|line| {
            nearest_caret(line, x_px)
                .map(|caret| caret.byte_index)
                .or(Some(line.byte_range.start))
        })
    }

    pub fn caret_position(&self, byte_index: usize) -> (f32, f32) {
        let Some(line) = self.line_for_byte(byte_index) else {
            return (0.0, 0.0);
        };
        let x = x_for_byte(line, byte_index);
        (x, line.top_px)
    }

    pub fn caret_rect(&self, byte_index: usize, fallback_height_px: f32) -> CaretRect {
        let Some(line) = self.line_for_byte(byte_index) else {
            return CaretRect {
                x_px: 0.0,
                y_px: 0.0,
                height_px: fallback_height_px,
            };
        };
        CaretRect {
            x_px: x_for_byte(line, byte_index),
            y_px: line.top_px,
            height_px: line.height_px,
        }
    }

    pub fn caret_rect_on_line(
        &self,
        line_index: usize,
        byte_index: usize,
        fallback_height_px: f32,
    ) -> CaretRect {
        let Some(line) = self.lines.get(line_index) else {
            return self.caret_rect(byte_index, fallback_height_px);
        };
        if byte_index < line.byte_range.start || byte_index > line.byte_range.end {
            return self.caret_rect(byte_index, fallback_height_px);
        }
        CaretRect {
            x_px: x_for_byte(line, byte_index),
            y_px: line.top_px,
            height_px: line.height_px,
        }
    }

    pub fn selection_spans(&self, range: Range<usize>) -> Vec<SelectionSpan> {
        if range.start >= range.end {
            return Vec::new();
        }

        let mut spans = Vec::new();
        for (line_index, line) in self.lines.iter().enumerate() {
            let start = range.start.max(line.byte_range.start);
            let end = range.end.min(line.byte_range.end);
            if start > end || (start == end && line.byte_range.start != line.byte_range.end) {
                continue;
            }
            if line.byte_range.start == line.byte_range.end
                && !(range.start <= line.byte_range.start && range.end >= line.byte_range.end)
            {
                continue;
            }

            let x0 = x_for_byte(line, start);
            let mut x1 = x_for_byte(line, end);
            if range.end > line.byte_range.end {
                x1 = x1
                    .max(self.width_px)
                    .max(line.width_px + line.height_px * 0.35);
            }
            if line.byte_range.start == line.byte_range.end
                && range.start <= line.byte_range.start
                && range.end >= line.byte_range.end
            {
                x1 = x1.max(self.width_px).max(x0 + line.height_px * 0.35);
            }
            if x1 <= x0 {
                continue;
            }
            spans.push(SelectionSpan {
                line: line_index,
                x_px: x0,
                y_px: line.top_px,
                width_px: x1 - x0,
                height_px: line.height_px,
            });
        }
        spans
    }

    fn line_for_byte(&self, byte_index: usize) -> Option<&TextLayoutLine> {
        self.line_index_for_byte(byte_index)
            .and_then(|index| self.lines.get(index))
    }

    fn line_index_for_y(&self, y_px: f32) -> Option<usize> {
        let first = self.lines.first()?;
        if y_px <= first.top_px {
            return Some(0);
        }

        for (index, line) in self.lines.iter().enumerate() {
            let bottom = line.top_px + line.height_px;
            let is_last = index + 1 == self.lines.len();
            if y_px >= line.top_px && (y_px < bottom || (is_last && y_px <= bottom)) {
                return Some(index);
            }
            if y_px < line.top_px {
                return Some(index.saturating_sub(1));
            }
        }

        Some(self.lines.len() - 1)
    }
}

fn nearest_caret(line: &TextLayoutLine, x_px: f32) -> Option<&CaretStop> {
    line.carets
        .iter()
        .min_by(|a, b| (a.x_px - x_px).abs().total_cmp(&(b.x_px - x_px).abs()))
}

fn x_for_byte(line: &TextLayoutLine, byte_index: usize) -> f32 {
    if let Some(caret) = line
        .carets
        .iter()
        .find(|caret| caret.byte_index == byte_index)
    {
        return caret.x_px;
    }

    let mut before = line.carets.first().map(|caret| caret.x_px).unwrap_or(0.0);
    for pair in line.carets.windows(2) {
        if byte_index >= pair[0].byte_index && byte_index <= pair[1].byte_index {
            return pair[0].x_px;
        }
        before = pair[1].x_px;
    }
    before
}

fn grapheme_boundaries(content: &str) -> Vec<usize> {
    let mut boundaries = Vec::new();
    boundaries.push(0);
    for (start, grapheme) in content.grapheme_indices(true) {
        if boundaries.last().copied() != Some(start) {
            boundaries.push(start);
        }
        let end = start + grapheme.len();
        if boundaries.last().copied() != Some(end) {
            boundaries.push(end);
        }
    }
    if boundaries.last().copied() != Some(content.len()) {
        boundaries.push(content.len());
    }
    boundaries
}

fn grapheme_floor(boundaries: &[usize], index: usize) -> usize {
    let boundary_index = boundaries.partition_point(|boundary| *boundary <= index);
    boundaries
        .get(boundary_index.saturating_sub(1))
        .copied()
        .unwrap_or(0)
}

fn next_grapheme_boundary(boundaries: &[usize], index: usize) -> usize {
    let floor = grapheme_floor(boundaries, index);
    let boundary_index = boundaries.partition_point(|boundary| *boundary <= floor);
    boundaries
        .get(boundary_index)
        .copied()
        .unwrap_or_else(|| boundaries.last().copied().unwrap_or(0))
}

/// One glyph pushed into the frame buffer; `x`/`y` are baseline pen coordinates in pixels.
pub struct PushedGlyph {
    pub glyph_id: u32,
    pub x: f32,
    pub y: f32,
    pub color: [f32; 4],
    pub(crate) info: GlyphInfo,
    size_px: f32,
}

/// One color glyph routed to the emoji atlas; `x`/`y` are baseline pen pixels, the
/// quad is a `size_px` em-square on the baseline, textured from `slot`.
struct PushedEmoji {
    x: f32,
    y: f32,
    size_px: f32,
    slot: EmojiSlot,
}

struct CachedShape {
    glyphs: Vec<ShapedGlyph>,
    line_breaks: Vec<u32>,
    line_ranges: Vec<Range<usize>>,
    line_advances: Vec<f32>,
    line_carets: Vec<Vec<LayoutCaretStop>>,
    block_width_em: f32,
    line_height_em: f32,
    line_count: u32,
}

#[derive(Clone)]
struct CachedParagraph {
    glyphs: Vec<ShapedGlyph>,
    last_used_frame: u64,
}

#[derive(Clone)]
struct CachedParagraphLayout {
    lines: Vec<LayoutLine>,
    last_used_frame: u64,
}

#[derive(Clone)]
struct LayoutLine {
    source: Range<usize>,
    glyphs: Vec<ShapedGlyph>,
    advance: f32,
    carets: Vec<LayoutCaretStop>,
}

#[derive(Clone, Debug)]
struct LayoutCaretStop {
    byte_index: usize,
    x_em: f32,
}

pub struct TextEngine {
    fonts: FontSet,
    glyph_cache: GlyphCache,
    paragraph_cache: HashMap<ParagraphKey, CachedParagraph>,
    paragraph_layout_cache: HashMap<ParagraphLayoutKey, CachedParagraphLayout>,
    frame: Vec<PushedGlyph>,
    emoji_cache: EmojiCache,
    emoji_frame: Vec<PushedEmoji>,
    vertex_scratch: Vec<TextVertex>,
    emoji_vertex_scratch: Vec<EmojiVertex>,
    frame_counter: u64,
}

fn align_offset_em(align: Align, block_width_em: f32, line_advance_em: f32) -> f32 {
    match align {
        Align::Left => 0.0,
        Align::Center => (block_width_em - line_advance_em) * 0.5,
        Align::Right => block_width_em - line_advance_em,
    }
}

fn line_glyph_range(line_breaks: &[u32], line_index: usize, glyph_count: usize) -> Range<usize> {
    let start = line_breaks
        .get(line_index)
        .copied()
        .unwrap_or(glyph_count as u32) as usize;
    let end = line_breaks
        .get(line_index + 1)
        .copied()
        .unwrap_or(glyph_count as u32) as usize;
    start.min(glyph_count)..end.min(glyph_count)
}

fn layout_lines_to_cache(
    lines: Vec<LayoutLine>,
    line_height_em: f32,
    block_width_em: f32,
) -> CachedShape {
    let mut glyphs = Vec::new();
    let mut line_breaks = Vec::new();
    let line_count = lines.len() as u32;
    let mut line_ranges = Vec::with_capacity(lines.len());
    let mut line_advances = Vec::with_capacity(lines.len());
    let mut line_carets = Vec::with_capacity(lines.len());
    let mut y_em = 0.0f32;

    for line in lines {
        line_breaks.push(glyphs.len() as u32);
        line_ranges.push(line.source.clone());
        line_advances.push(line.advance);
        line_carets.push(line.carets);
        glyphs.extend(line.glyphs.into_iter().map(|mut glyph| {
            glyph.y = y_em;
            glyph
        }));
        y_em -= line_height_em;
    }

    CachedShape {
        glyphs,
        line_breaks,
        line_ranges,
        line_advances,
        line_carets,
        block_width_em,
        line_height_em,
        line_count,
    }
}

fn wrap_shaped_paragraph(
    paragraph: &str,
    paragraph_start: usize,
    glyphs: &[ShapedGlyph],
    max_width_em: f32,
    out: &mut Vec<LayoutLine>,
) {
    if paragraph.is_empty() {
        out.push(LayoutLine {
            source: paragraph_start..paragraph_start,
            glyphs: Vec::new(),
            advance: 0.0,
            carets: Vec::new(),
        });
        return;
    }

    let grapheme_boundaries = grapheme_boundaries(paragraph);
    let mut current_start: Option<usize> = None;
    let mut current_end = 0usize;
    let mut current_origin = 0.0f32;
    let mut current_width = 0.0f32;
    let mut current_glyphs = Vec::new();

    for (token_start, token) in tokenize(paragraph) {
        let token_end = token_start + token.len();
        let is_ws = token.chars().all(char::is_whitespace);
        let token_glyphs = glyphs
            .iter()
            .copied()
            .filter(|glyph| glyph.cluster >= token_start && glyph.cluster < token_end)
            .collect::<Vec<_>>();
        let token_origin = token_glyphs
            .first()
            .map(|glyph| glyph.x)
            .unwrap_or(current_origin + current_width);
        let token_end_x = token_glyphs
            .last()
            .map(|glyph| glyph.x + glyph.advance_x)
            .unwrap_or(token_origin);
        let token_width = token_end_x - token_origin;

        if token_width > max_width_em && token_glyphs.len() > 1 {
            if let Some(start) = current_start.take() {
                flush_layout_line(
                    paragraph_start,
                    start,
                    current_end,
                    current_origin,
                    current_width,
                    &mut current_glyphs,
                    out,
                );
            }
            flush_wrapped_token_lines(
                paragraph_start,
                token_start,
                token_end,
                token_origin,
                token_glyphs,
                max_width_em,
                &grapheme_boundaries,
                out,
            );
            current_end = token_end;
            current_origin = token_end_x;
            current_width = 0.0;
            continue;
        }

        if current_start.is_none() {
            current_start = Some(token_start);
            current_end = token_end;
            current_origin = token_origin;
            current_width = token_end_x - current_origin;
            current_glyphs.extend(token_glyphs);
            continue;
        }

        let proposed_width = token_end_x - current_origin;
        if proposed_width <= max_width_em || current_width <= 0.0 {
            current_end = token_end;
            current_width = proposed_width;
            current_glyphs.extend(token_glyphs);
            continue;
        }

        flush_layout_line(
            paragraph_start,
            current_start.take().unwrap(),
            current_end,
            current_origin,
            current_width,
            &mut current_glyphs,
            out,
        );

        if !is_ws {
            current_start = Some(token_start);
            current_end = token_end;
            current_origin = token_origin;
            current_width = token_end_x - current_origin;
            current_glyphs.extend(token_glyphs);
        } else {
            current_width = 0.0;
        }
    }

    if let Some(start) = current_start {
        flush_layout_line(
            paragraph_start,
            start,
            current_end,
            current_origin,
            current_width,
            &mut current_glyphs,
            out,
        );
    }
}

fn flush_wrapped_token_lines(
    paragraph_start: usize,
    token_start: usize,
    token_end: usize,
    token_origin: f32,
    token_glyphs: Vec<ShapedGlyph>,
    max_width_em: f32,
    grapheme_boundaries: &[usize],
    out: &mut Vec<LayoutLine>,
) {
    let mut line_start = token_start;
    let mut line_end = token_start;
    let mut line_origin = token_origin;
    let mut line_width = 0.0f32;
    let mut line_glyphs = Vec::new();

    for glyph in token_glyphs {
        let glyph_right = glyph.x + glyph.advance_x;
        let proposed_width = glyph_right - line_origin;
        if !line_glyphs.is_empty() && proposed_width > max_width_em {
            let break_byte = grapheme_floor(grapheme_boundaries, glyph.cluster);
            if break_byte <= line_start {
                line_width = proposed_width;
                line_end = token_end;
                line_glyphs.push(glyph);
                continue;
            }
            flush_layout_line(
                paragraph_start,
                line_start,
                break_byte,
                line_origin,
                line_width,
                &mut line_glyphs,
                out,
            );
            line_start = break_byte;
            line_origin = glyph.x;
            line_width = glyph.advance_x;
        } else {
            line_width = proposed_width;
        }
        line_end = token_end;
        line_glyphs.push(glyph);
    }

    if !line_glyphs.is_empty() {
        flush_layout_line(
            paragraph_start,
            line_start,
            line_end,
            line_origin,
            line_width,
            &mut line_glyphs,
            out,
        );
    }
}

fn flush_layout_line(
    paragraph_start: usize,
    start: usize,
    end: usize,
    origin: f32,
    width: f32,
    glyphs: &mut Vec<ShapedGlyph>,
    out: &mut Vec<LayoutLine>,
) {
    let line_glyphs = glyphs
        .drain(..)
        .map(|mut glyph| {
            glyph.x -= origin;
            glyph.cluster += paragraph_start;
            glyph
        })
        .collect();
    out.push(LayoutLine {
        source: paragraph_start + start..paragraph_start + end,
        glyphs: line_glyphs,
        advance: width.max(0.0),
        carets: Vec::new(),
    });
}

fn attach_carets_to_lines(lines: &mut [LayoutLine], paragraph: &str, paragraph_start: usize) {
    let grapheme_boundaries = grapheme_boundaries(paragraph)
        .into_iter()
        .map(|boundary| paragraph_start + boundary)
        .collect::<Vec<_>>();

    for line in lines {
        let mut carets = Vec::new();
        carets.push(LayoutCaretStop {
            byte_index: line.source.start,
            x_em: 0.0,
        });
        for glyph in &line.glyphs {
            let glyph_start = grapheme_floor(&grapheme_boundaries, glyph.cluster);
            if carets
                .last()
                .is_none_or(|caret| caret.byte_index != glyph_start)
            {
                carets.push(LayoutCaretStop {
                    byte_index: glyph_start,
                    x_em: glyph.x,
                });
            }
            let after = next_grapheme_boundary(&grapheme_boundaries, glyph.cluster);
            if after <= line.source.end
                && carets.last().is_none_or(|caret| caret.byte_index != after)
            {
                carets.push(LayoutCaretStop {
                    byte_index: after,
                    x_em: glyph.x + glyph.advance_x,
                });
            }
        }
        carets.push(LayoutCaretStop {
            byte_index: line.source.end,
            x_em: line.advance,
        });
        carets.sort_by_key(|caret| caret.byte_index);
        carets.dedup_by_key(|caret| caret.byte_index);
        line.carets = carets;
    }
}

fn offset_layout_line(mut line: LayoutLine, byte_offset: usize) -> LayoutLine {
    line.source = line.source.start + byte_offset..line.source.end + byte_offset;
    for glyph in &mut line.glyphs {
        glyph.cluster += byte_offset;
    }
    for caret in &mut line.carets {
        caret.byte_index += byte_offset;
    }
    line
}

fn tokenize(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    let bytes = text.as_bytes();
    std::iter::from_fn(move || {
        if start >= bytes.len() {
            return None;
        }
        let mut i = start;
        while i < bytes.len() {
            let ch = text[i..].chars().next().unwrap();
            let ws = ch.is_whitespace();
            match prev_ws {
                None => prev_ws = Some(ws),
                Some(prev) if prev != ws => {
                    let token_start = start;
                    let token = &text[token_start..i];
                    start = i;
                    prev_ws = Some(ws);
                    return Some((token_start, token));
                }
                _ => {}
            }
            i += ch.len_utf8();
        }
        let token_start = start;
        let token = &text[token_start..];
        start = bytes.len();
        prev_ws = None;
        Some((token_start, token))
    })
}

impl TextEngine {
    fn build_shape(&mut self, content: &str, args: &TextArgs) -> CachedShape {
        self.build_shape_with_paragraphs(content, args, None)
    }

    fn build_shape_from_lines(&mut self, lines: Vec<LayoutLine>, args: &TextArgs) -> CachedShape {
        let size_px = args.size_px.max(0.0001);
        let max_width_em = args.max_width_px.map(|w| w / size_px);
        let metrics = self.fonts.primary().metrics();
        let line_height_em = metrics.line_height() * args.line_spacing;
        let block_width_em = if let Some(w_em) = max_width_em {
            w_em
        } else {
            lines.iter().map(|l| l.advance).fold(0.0f32, f32::max)
        };
        layout_lines_to_cache(lines, line_height_em, block_width_em)
    }

    fn build_shape_with_paragraphs(
        &mut self,
        content: &str,
        args: &TextArgs,
        paragraph_identities: Option<&[ParagraphIdentity]>,
    ) -> CachedShape {
        let size_px = args.size_px.max(0.0001);

        let max_width_em = args.max_width_px.map(|w| w / size_px);
        let max_w_em = max_width_em.unwrap_or(f32::MAX).max(0.0);
        let lines = self.layout_paragraphs(content, max_w_em, paragraph_identities);
        self.build_shape_from_lines(lines, args)
    }

    fn build_shape_from_paragraph_runs(
        &mut self,
        paragraphs: &[TextParagraph<'_>],
        args: &TextArgs,
    ) -> CachedShape {
        let size_px = args.size_px.max(0.0001);
        let max_width_em = args.max_width_px.map(|w| w / size_px);
        let max_w_em = max_width_em.unwrap_or(f32::MAX).max(0.0);
        let lines = self.layout_paragraph_run_lines(paragraphs, max_w_em);
        self.build_shape_from_lines(lines, args)
    }

    fn build_shape_from_paragraph_identities(
        &mut self,
        paragraphs: &[TextParagraphIdentity],
        provider: &mut impl TextParagraphProvider,
        args: &TextArgs,
    ) -> Option<CachedShape> {
        let size_px = args.size_px.max(0.0001);
        let max_width_em = args.max_width_px.map(|w| w / size_px);
        let max_w_em = max_width_em.unwrap_or(f32::MAX).max(0.0);
        let lines = self.layout_paragraph_identity_lines(paragraphs, provider, max_w_em)?;
        Some(self.build_shape_from_lines(lines, args))
    }

    fn layout_paragraphs(
        &mut self,
        content: &str,
        max_width_em: f32,
        paragraph_identities: Option<&[ParagraphIdentity]>,
    ) -> Vec<LayoutLine> {
        let mut lines = Vec::new();
        let mut paragraph_start = 0usize;
        let paragraph_count = content.split('\n').count();
        let identities =
            paragraph_identities.filter(|identities| identities.len() == paragraph_count);
        for (paragraph_index, paragraph) in content.split('\n').enumerate() {
            let key = identities
                .and_then(|identities| identities.get(paragraph_index).copied())
                .map(ParagraphKey::identity)
                .unwrap_or_else(|| ParagraphKey::content(paragraph));
            let paragraph_lines = self.cached_paragraph_layout(paragraph, key, max_width_em);
            lines.extend(
                paragraph_lines
                    .into_iter()
                    .map(|line| offset_layout_line(line, paragraph_start)),
            );
            paragraph_start += paragraph.len() + 1;
        }
        lines
    }

    fn layout_paragraph_run_lines(
        &mut self,
        paragraphs: &[TextParagraph<'_>],
        max_width_em: f32,
    ) -> Vec<LayoutLine> {
        let mut lines = Vec::new();
        for paragraph in paragraphs {
            let key = paragraph
                .identity
                .map(ParagraphKey::identity)
                .unwrap_or_else(|| ParagraphKey::content(paragraph.text));
            let paragraph_lines = self.cached_paragraph_layout(paragraph.text, key, max_width_em);
            lines.extend(
                paragraph_lines
                    .into_iter()
                    .map(|line| offset_layout_line(line, paragraph.byte_start)),
            );
        }
        lines
    }

    fn layout_paragraph_identity_lines(
        &mut self,
        paragraphs: &[TextParagraphIdentity],
        provider: &mut impl TextParagraphProvider,
        max_width_em: f32,
    ) -> Option<Vec<LayoutLine>> {
        let mut lines = Vec::new();
        for paragraph in paragraphs {
            let key = ParagraphKey::identity(paragraph.identity);
            let paragraph_lines =
                self.cached_paragraph_layout_by_identity(paragraph, key, provider, max_width_em)?;
            lines.extend(
                paragraph_lines
                    .into_iter()
                    .map(|line| offset_layout_line(line, paragraph.byte_start)),
            );
        }
        Some(lines)
    }

    fn cached_paragraph_layout(
        &mut self,
        paragraph: &str,
        key: ParagraphKey,
        max_width_em: f32,
    ) -> Vec<LayoutLine> {
        let key = ParagraphLayoutKey::new(key, max_width_em);
        if let Some(cached) = self.paragraph_layout_cache.get_mut(&key) {
            cached.last_used_frame = self.frame_counter;
            return cached.lines.clone();
        }

        let glyphs = self.cached_paragraph_glyphs(paragraph, key.paragraph.clone());
        let mut lines = Vec::new();
        wrap_shaped_paragraph(paragraph, 0, glyphs.as_slice(), max_width_em, &mut lines);
        attach_carets_to_lines(&mut lines, paragraph, 0);
        self.paragraph_layout_cache.insert(
            key,
            CachedParagraphLayout {
                lines: lines.clone(),
                last_used_frame: self.frame_counter,
            },
        );
        lines
    }

    fn cached_paragraph_layout_by_identity(
        &mut self,
        paragraph: &TextParagraphIdentity,
        key: ParagraphKey,
        provider: &mut impl TextParagraphProvider,
        max_width_em: f32,
    ) -> Option<Vec<LayoutLine>> {
        let key = ParagraphLayoutKey::new(key, max_width_em);
        if let Some(cached) = self.paragraph_layout_cache.get_mut(&key) {
            cached.last_used_frame = self.frame_counter;
            return Some(cached.lines.clone());
        }

        let text = provider.paragraph_text(paragraph.identity)?;
        if text.len() != paragraph.byte_len {
            return None;
        }
        let glyphs = self.cached_paragraph_glyphs(text.as_ref(), key.paragraph.clone());
        let mut lines = Vec::new();
        wrap_shaped_paragraph(
            text.as_ref(),
            0,
            glyphs.as_slice(),
            max_width_em,
            &mut lines,
        );
        attach_carets_to_lines(&mut lines, text.as_ref(), 0);
        self.paragraph_layout_cache.insert(
            key,
            CachedParagraphLayout {
                lines: lines.clone(),
                last_used_frame: self.frame_counter,
            },
        );
        Some(lines)
    }

    fn cached_paragraph_glyphs(&mut self, paragraph: &str, key: ParagraphKey) -> Vec<ShapedGlyph> {
        if let Some(cached) = self.paragraph_cache.get_mut(&key) {
            cached.last_used_frame = self.frame_counter;
            return cached.glyphs.clone();
        }

        let run = shape_text(&self.fonts, &mut self.glyph_cache, paragraph, 0.0, 0.0);
        let glyphs = run.glyphs;
        self.paragraph_cache.insert(
            key,
            CachedParagraph {
                glyphs: glyphs.clone(),
                last_used_frame: self.frame_counter,
            },
        );
        glyphs
    }

    pub fn load(font_path: &str) -> Result<Self, String> {
        Self::from_sources(vec![FontSource::Path(font_path)])
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        Self::from_sources(vec![FontSource::Bytes(bytes, 0)])
    }

    /// Build from an ordered fallback chain: the first source is primary, the rest
    /// are fallbacks for characters it lacks. Sources that fail to load are skipped.
    pub fn from_sources(sources: Vec<FontSource<'_>>) -> Result<Self, String> {
        FontSet::load(sources).map(Self::from_font_set)
    }

    fn from_font_set(fonts: FontSet) -> Self {
        Self {
            fonts,
            glyph_cache: GlyphCache::new(),
            paragraph_cache: HashMap::new(),
            paragraph_layout_cache: HashMap::new(),
            frame: Vec::new(),
            emoji_cache: EmojiCache::new(),
            emoji_frame: Vec::new(),
            vertex_scratch: Vec::new(),
            emoji_vertex_scratch: Vec::new(),
            frame_counter: 0,
        }
    }

    pub fn metrics(&self) -> FontMetrics {
        self.fonts.primary().metrics()
    }

    pub fn units_per_em(&self) -> u16 {
        self.fonts.primary().units_per_em()
    }

    /// Characters in `text` that no face in the fallback chain covers — these
    /// render as `.notdef` tofu (whitespace/control excluded). Diagnostic aid.
    pub fn uncovered_chars(&self, text: &str) -> Vec<char> {
        text.chars()
            .filter(|c| {
                !c.is_whitespace() && !c.is_control() && self.fonts.covering_face(*c).is_none()
            })
            .collect()
    }

    /// Whether any face in the fallback chain has a glyph for `c` (i.e. it would
    /// render as something other than `.notdef` tofu). Useful for enumerating the
    /// renderable subset of a code range.
    pub fn covers(&self, c: char) -> bool {
        self.fonts.covering_face(c).is_some()
    }

    /// Em-space ink bounding box `(min_x, min_y, max_x, max_y)` of the glyph `c`
    /// resolves to through fallback. `None` for whitespace, uncovered characters,
    /// and color (emoji) glyphs. Handy for fitting a glyph into a fixed cell —
    /// note some glyphs (Private Use icons, Cuneiform, …) extend well beyond the
    /// `[0,1]` em box, so a fixed size will overflow unless scaled to this.
    pub fn glyph_bbox(&mut self, c: char) -> Option<(f32, f32, f32, f32)> {
        let mut buf = [0u8; 4];
        let run = shape_text(&self.fonts, &mut self.glyph_cache, c.encode_utf8(&mut buf), 0.0, 0.0);
        run.glyphs.iter().find_map(|g| g.info.map(|i| i.bbox))
    }

    /// Family names of the loaded fallback chain, in fallback order (diagnostics).
    pub fn fallback_family_names(&self) -> Vec<String> {
        self.fonts.family_names()
    }

    /// Family name of the face that would render `c`, resolving presentation the
    /// same way shaping does (emoji-default → a color face). `None` if uncovered.
    pub fn family_for(&self, c: char) -> Option<String> {
        let id = if crate::emoji_presentation::is_default_emoji(c) {
            self.fonts.color_face_for(c)
        } else {
            self.fonts.text_face_for(c)
        }
        .or_else(|| self.fonts.covering_face(c))?;
        self.fonts.face(id).family_name()
    }

    /// Curve atlas dimensions `(width, height)` for debugging / diagnostics.
    pub fn curve_atlas_size(&self) -> (u32, u32) {
        self.glyph_cache.curve_size()
    }

    /// Band atlas dimensions `(width, height)` for debugging / diagnostics.
    pub fn band_atlas_size(&self) -> (u32, u32) {
        self.glyph_cache.band_size()
    }

    pub fn new_atlas(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
    ) -> TextAtlas {
        TextAtlas::new(device, queue, layout, &self.glyph_cache)
    }

    pub fn sync_atlas(
        &self,
        atlas: &mut TextAtlas,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
    ) {
        TextAtlas::sync(atlas, device, queue, layout, &self.glyph_cache);
    }

    /// Emoji quads produced by the last [`flush`](Self::flush), valid until the
    /// next flush. Drawn with [`EmojiRenderer`](crate::EmojiRenderer) over the text.
    pub fn emoji_vertices(&self) -> &[EmojiVertex] {
        &self.emoji_vertex_scratch
    }

    /// Number of color glyphs pushed so far this frame (before flush). Snapshot
    /// around a `text*` call to get that call's emoji range for per-item drawing.
    pub fn emoji_len(&self) -> usize {
        self.emoji_frame.len()
    }

    pub fn new_emoji_atlas(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
    ) -> EmojiAtlas {
        EmojiAtlas::new(device, queue, layout, &self.emoji_cache)
    }

    pub fn sync_emoji_atlas(
        &self,
        atlas: &mut EmojiAtlas,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
    ) {
        EmojiAtlas::sync(atlas, device, queue, layout, &self.emoji_cache);
    }


    pub fn layout(&mut self, content: &str, args: &TextArgs) -> TextLayout {
        let shape = self.build_shape(content, args);
        Self::layout_shape(self.fonts.primary(), args, shape)
    }

    pub fn layout_with_paragraphs(
        &mut self,
        content: &str,
        paragraph_identities: &[ParagraphIdentity],
        args: &TextArgs,
    ) -> TextLayout {
        let shape = self.build_shape_with_paragraphs(content, args, Some(paragraph_identities));
        Self::layout_shape(self.fonts.primary(), args, shape)
    }

    pub fn layout_paragraph_runs(
        &mut self,
        paragraphs: &[TextParagraph<'_>],
        args: &TextArgs,
    ) -> TextLayout {
        let shape = self.build_shape_from_paragraph_runs(paragraphs, args);
        Self::layout_shape(self.fonts.primary(), args, shape)
    }

    pub fn layout_paragraph_identities(
        &mut self,
        paragraphs: &[TextParagraphIdentity],
        provider: &mut impl TextParagraphProvider,
        args: &TextArgs,
    ) -> Option<TextLayout> {
        let shape = self.build_shape_from_paragraph_identities(paragraphs, provider, args)?;
        Some(Self::layout_shape(self.fonts.primary(), args, shape))
    }

    fn layout_shape(font: &Font, args: &TextArgs, shape: CachedShape) -> TextLayout {
        let size_px = args.size_px.max(0.0001);
        let metrics = font.metrics();
        let line_height_px = shape.line_height_em * size_px;
        let ascent_px = metrics.ascent * size_px;
        let mut lines = Vec::with_capacity(shape.line_ranges.len());

        for (line_index, byte_range) in shape.line_ranges.iter().enumerate() {
            let line_advance = shape.line_advances[line_index];
            let align_x = align_offset_em(args.align, shape.block_width_em, line_advance) * size_px;
            let top_px = line_index as f32 * line_height_px;
            let baseline_px = top_px + ascent_px;
            let carets = shape.line_carets[line_index]
                .iter()
                .map(|caret| CaretStop {
                    byte_index: caret.byte_index,
                    x_px: align_x + caret.x_em * size_px,
                })
                .collect();

            lines.push(TextLayoutLine {
                byte_range: byte_range.clone(),
                top_px,
                baseline_px,
                height_px: line_height_px,
                width_px: line_advance * size_px,
                carets,
            });
        }

        TextLayout {
            lines,
            width_px: shape.block_width_em * size_px,
            height_px: shape.line_count as f32 * line_height_px,
            glyphs: shape.glyphs,
            line_breaks: shape.line_breaks,
            line_advances: shape.line_advances,
            block_width_em: shape.block_width_em,
        }
    }

    pub fn text(&mut self, x: f32, y: f32, content: &str, args: &TextArgs) -> &mut [PushedGlyph] {
        let shape = self.build_shape(content, args);
        self.push_shape_clipped(x, y, shape, args, None)
    }

    pub fn text_clipped(
        &mut self,
        x: f32,
        y: f32,
        content: &str,
        args: &TextArgs,
        clip: TextClip,
    ) -> &mut [PushedGlyph] {
        let shape = self.build_shape(content, args);
        self.push_shape_clipped(x, y, shape, args, Some(clip))
    }

    pub fn text_with_paragraphs(
        &mut self,
        x: f32,
        y: f32,
        content: &str,
        paragraph_identities: &[ParagraphIdentity],
        args: &TextArgs,
    ) -> &mut [PushedGlyph] {
        let shape = self.build_shape_with_paragraphs(content, args, Some(paragraph_identities));
        self.push_shape_clipped(x, y, shape, args, None)
    }

    pub fn text_with_paragraphs_clipped(
        &mut self,
        x: f32,
        y: f32,
        content: &str,
        paragraph_identities: &[ParagraphIdentity],
        args: &TextArgs,
        clip: TextClip,
    ) -> &mut [PushedGlyph] {
        let shape = self.build_shape_with_paragraphs(content, args, Some(paragraph_identities));
        self.push_shape_clipped(x, y, shape, args, Some(clip))
    }

    pub fn text_paragraph_runs(
        &mut self,
        x: f32,
        y: f32,
        paragraphs: &[TextParagraph<'_>],
        args: &TextArgs,
    ) -> &mut [PushedGlyph] {
        let shape = self.build_shape_from_paragraph_runs(paragraphs, args);
        self.push_shape_clipped(x, y, shape, args, None)
    }

    pub fn text_paragraph_runs_clipped(
        &mut self,
        x: f32,
        y: f32,
        paragraphs: &[TextParagraph<'_>],
        args: &TextArgs,
        clip: TextClip,
    ) -> &mut [PushedGlyph] {
        let shape = self.build_shape_from_paragraph_runs(paragraphs, args);
        self.push_shape_clipped(x, y, shape, args, Some(clip))
    }

    pub fn text_paragraph_identities(
        &mut self,
        x: f32,
        y: f32,
        paragraphs: &[TextParagraphIdentity],
        provider: &mut impl TextParagraphProvider,
        args: &TextArgs,
    ) -> Option<&mut [PushedGlyph]> {
        let shape = self.build_shape_from_paragraph_identities(paragraphs, provider, args)?;
        Some(self.push_shape_clipped(x, y, shape, args, None))
    }

    pub fn text_paragraph_identities_clipped(
        &mut self,
        x: f32,
        y: f32,
        paragraphs: &[TextParagraphIdentity],
        provider: &mut impl TextParagraphProvider,
        args: &TextArgs,
        clip: TextClip,
    ) -> Option<&mut [PushedGlyph]> {
        let shape = self.build_shape_from_paragraph_identities(paragraphs, provider, args)?;
        Some(self.push_shape_clipped(x, y, shape, args, Some(clip)))
    }

    /// Route a color glyph to the emoji atlas: rasterize once per size bucket and
    /// queue a textured quad. Skips glyphs that aren't renderable color glyphs or
    /// whose em-box falls outside the vertical clip.
    fn push_emoji_glyph(
        &mut self,
        face_id: u16,
        glyph_id: u32,
        px: f32,
        py: f32,
        size_px: f32,
        clip: Option<TextClip>,
    ) {
        if clip.is_some_and(|clip| !clip.intersects_y(py - size_px, py)) {
            return;
        }
        let bucket = bucket_for(size_px);
        let face = self.fonts.face(face_id).face();
        if let Some(slot) = self.emoji_cache.get_or_insert(face, face_id, glyph_id, bucket) {
            self.emoji_frame.push(PushedEmoji {
                x: px,
                y: py,
                size_px,
                slot,
            });
        }
    }

    fn push_shape_clipped(
        &mut self,
        x: f32,
        y: f32,
        shape: CachedShape,
        args: &TextArgs,
        clip: Option<TextClip>,
    ) -> &mut [PushedGlyph] {
        let size_px = args.size_px.max(0.0001);
        let line_height_px = shape.line_height_em * size_px;
        let ascent_px = self.fonts.primary().metrics().ascent * size_px;
        let glyph_count = shape.glyphs.len();

        let start = self.frame.len();
        for (line_index, adv) in shape.line_advances.iter().copied().enumerate() {
            let line_top = y - ascent_px + line_index as f32 * line_height_px;
            let line_bottom = line_top + line_height_px;
            if clip.is_some_and(|clip| !clip.intersects_y(line_top, line_bottom)) {
                continue;
            }

            let ox = align_offset_em(args.align, shape.block_width_em, adv);
            for i in line_glyph_range(&shape.line_breaks, line_index, glyph_count) {
                let g = &shape.glyphs[i];
                let gx = g.x + ox;
                let px = x + gx * size_px;
                let py = y - g.y * size_px;
                if g.is_color {
                    self.push_emoji_glyph(g.face_id, g.glyph_id, px, py, size_px, clip);
                    continue;
                }
                let Some(info) = g.info else {
                    continue;
                };
                if clip.is_some_and(|clip| !clip.intersects_glyph(&info, px, py, size_px)) {
                    continue;
                }
                self.frame.push(PushedGlyph {
                    glyph_id: g.glyph_id,
                    x: px,
                    y: py,
                    color: args.color,
                    info,
                    size_px,
                });
            }
        }

        &mut self.frame[start..]
    }

    pub fn text_layout(
        &mut self,
        x: f32,
        y: f32,
        layout: &TextLayout,
        args: &TextArgs,
    ) -> &mut [PushedGlyph] {
        self.text_layout_clipped_inner(x, y, layout, args, None)
    }

    pub fn text_layout_clipped(
        &mut self,
        x: f32,
        y: f32,
        layout: &TextLayout,
        args: &TextArgs,
        clip: TextClip,
    ) -> &mut [PushedGlyph] {
        self.text_layout_clipped_inner(x, y, layout, args, Some(clip))
    }

    fn text_layout_clipped_inner(
        &mut self,
        x: f32,
        y: f32,
        layout: &TextLayout,
        args: &TextArgs,
        clip: Option<TextClip>,
    ) -> &mut [PushedGlyph] {
        let size_px = args.size_px.max(0.0001);
        let start = self.frame.len();
        let glyph_count = layout.glyphs.len();
        let block_top = layout
            .lines
            .first()
            .map(|line| y - line.baseline_px)
            .unwrap_or(y);

        for (line_index, adv) in layout.line_advances.iter().copied().enumerate() {
            if let Some(line) = layout.lines.get(line_index) {
                let line_top = block_top + line.top_px;
                let line_bottom = line_top + line.height_px;
                if clip.is_some_and(|clip| !clip.intersects_y(line_top, line_bottom)) {
                    continue;
                }
            }

            let ox = align_offset_em(args.align, layout.block_width_em, adv);
            for i in line_glyph_range(&layout.line_breaks, line_index, glyph_count) {
                let g = &layout.glyphs[i];
                let gx = g.x + ox;
                let px = x + gx * size_px;
                let py = y - g.y * size_px;
                if g.is_color {
                    self.push_emoji_glyph(g.face_id, g.glyph_id, px, py, size_px, clip);
                    continue;
                }
                let Some(info) = g.info else {
                    continue;
                };
                if clip.is_some_and(|clip| !clip.intersects_glyph(&info, px, py, size_px)) {
                    continue;
                }
                self.frame.push(PushedGlyph {
                    glyph_id: g.glyph_id,
                    x: px,
                    y: py,
                    color: args.color,
                    info,
                    size_px,
                });
            }
        }

        &mut self.frame[start..]
    }

    pub fn flush(&mut self) -> &[TextVertex] {
        self.vertex_scratch.clear();
        self.emoji_vertex_scratch.clear();

        for g in &self.frame {
            push_glyph_quad_pixels(
                &mut self.vertex_scratch,
                &g.info,
                g.x,
                g.y,
                g.size_px,
                g.color,
            );
        }
        for e in &self.emoji_frame {
            let uv_min = [e.slot.x as f32, e.slot.y as f32];
            let uv_max = [
                (e.slot.x + e.slot.size) as f32,
                (e.slot.y + e.slot.size) as f32,
            ];
            push_emoji_quad(
                &mut self.emoji_vertex_scratch,
                e.x,
                e.y,
                e.size_px,
                uv_min,
                uv_max,
            );
        }

        let fc = self.frame_counter;
        self.paragraph_cache.retain(|_, paragraph| {
            fc.saturating_sub(paragraph.last_used_frame) <= CACHE_TTL_FRAMES
        });
        self.paragraph_layout_cache.retain(|_, paragraph| {
            fc.saturating_sub(paragraph.last_used_frame) <= CACHE_TTL_FRAMES
        });

        self.frame.clear();
        self.emoji_frame.clear();
        self.frame_counter = self.frame_counter.wrapping_add(1);
        &self.vertex_scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> Option<TextEngine> {
        [
            "C:\\Windows\\Fonts\\segoeui.ttf",
            "C:\\Windows\\Fonts\\arial.ttf",
        ]
        .into_iter()
        .find_map(|path| TextEngine::load(path).ok())
    }

    #[test]
    fn long_unbroken_token_hard_wraps_on_valid_boundaries() {
        let Some(mut engine) = test_engine() else {
            return;
        };
        let text = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let layout = engine.layout(
            text,
            &TextArgs {
                size_px: 18.0,
                max_width_px: Some(72.0),
                ..TextArgs::default()
            },
        );

        assert!(layout.lines.len() > 1);
        assert_eq!(layout.lines.first().unwrap().byte_range.start, 0);
        assert_eq!(layout.lines.last().unwrap().byte_range.end, text.len());
        for pair in layout.lines.windows(2) {
            assert_eq!(pair[0].byte_range.end, pair[1].byte_range.start);
        }
        for line in &layout.lines {
            assert!(text.is_char_boundary(line.byte_range.start));
            assert!(text.is_char_boundary(line.byte_range.end));
        }
    }

    #[test]
    fn whitespace_only_paragraph_keeps_visual_line() {
        let Some(mut engine) = test_engine() else {
            return;
        };
        let text = "first line\n ";
        let layout = engine.layout(
            text,
            &TextArgs {
                size_px: 18.0,
                max_width_px: Some(240.0),
                ..TextArgs::default()
            },
        );

        let last = layout.lines.last().unwrap();
        assert_eq!(last.byte_range, "first line\n".len().."first line\n ".len());
        assert_eq!(
            layout.line_index_for_byte(text.len()),
            Some(layout.lines.len() - 1)
        );
        assert!(layout.caret_rect(text.len(), 18.0).x_px > 0.0);
    }

    struct CountingProvider<'a> {
        identity: ParagraphIdentity,
        text: &'a str,
        calls: usize,
    }

    impl TextParagraphProvider for CountingProvider<'_> {
        fn paragraph_text(&mut self, identity: ParagraphIdentity) -> Option<Cow<'_, str>> {
            (identity == self.identity).then(|| {
                self.calls += 1;
                Cow::Borrowed(self.text)
            })
        }
    }

    #[test]
    fn paragraph_identity_layout_cache_hit_does_not_request_bytes() {
        let Some(mut engine) = test_engine() else {
            return;
        };
        let identity = ParagraphIdentity {
            namespace: 7,
            slot: 11,
            generation: 13,
        };
        let text = "cached paragraph should only be fetched once";
        let paragraphs = [TextParagraphIdentity {
            identity,
            byte_start: 0,
            byte_len: text.len(),
        }];
        let args = TextArgs {
            size_px: 18.0,
            max_width_px: Some(220.0),
            ..TextArgs::default()
        };
        let mut provider = CountingProvider {
            identity,
            text,
            calls: 0,
        };

        let first = engine
            .layout_paragraph_identities(&paragraphs, &mut provider, &args)
            .unwrap();
        assert_eq!(provider.calls, 1);
        let second = engine
            .layout_paragraph_identities(&paragraphs, &mut provider, &args)
            .unwrap();

        assert_eq!(provider.calls, 1);
        assert_eq!(second.lines.len(), first.lines.len());
        assert_eq!(
            second.lines.last().unwrap().byte_range.end,
            first.lines.last().unwrap().byte_range.end
        );
    }

    #[test]
    fn clipped_text_layout_emits_only_visible_glyphs() {
        let Some(mut engine) = test_engine() else {
            return;
        };
        let text = "alpha bravo\ncharlie delta\necho foxtrot\ngolf hotel";
        let args = TextArgs {
            size_px: 18.0,
            max_width_px: Some(360.0),
            ..TextArgs::default()
        };
        let layout = engine.layout(text, &args);
        let baseline_y = layout.lines.first().unwrap().baseline_px;

        let all_glyphs = engine.text_layout(0.0, baseline_y, &layout, &args).len();
        assert!(all_glyphs > 0);
        assert_eq!(engine.flush().len(), all_glyphs * 6);

        let first_line = layout.lines.first().unwrap();
        let clip =
            TextClip::from_min_size(0.0, first_line.top_px, 360.0, first_line.height_px + 0.5)
                .unwrap();
        let clipped_glyphs = engine
            .text_layout_clipped(0.0, baseline_y, &layout, &args, clip)
            .len();

        assert!(clipped_glyphs > 0);
        assert!(clipped_glyphs < all_glyphs);
        assert_eq!(engine.flush().len(), clipped_glyphs * 6);
    }

    #[test]
    fn clipped_text_layout_emits_nothing_outside_horizontal_clip() {
        let Some(mut engine) = test_engine() else {
            return;
        };
        let text = "alpha bravo";
        let args = TextArgs {
            size_px: 18.0,
            max_width_px: Some(360.0),
            ..TextArgs::default()
        };
        let layout = engine.layout(text, &args);
        let baseline_y = layout.lines.first().unwrap().baseline_px;
        let clip = TextClip::from_min_size(10_000.0, 0.0, 100.0, 100.0).unwrap();

        let clipped_glyphs = engine
            .text_layout_clipped(0.0, baseline_y, &layout, &args, clip)
            .len();

        assert_eq!(clipped_glyphs, 0);
        assert!(engine.flush().is_empty());
    }

    #[test]
    fn clipped_text_emits_only_visible_shape_lines() {
        let Some(mut engine) = test_engine() else {
            return;
        };
        let text = "alpha bravo\ncharlie delta\necho foxtrot\ngolf hotel";
        let args = TextArgs {
            size_px: 18.0,
            max_width_px: Some(360.0),
            ..TextArgs::default()
        };
        let baseline_y = engine.metrics().ascent * args.size_px;

        let all_glyphs = engine.text(0.0, baseline_y, text, &args).len();
        assert!(all_glyphs > 0);
        let _ = engine.flush();

        let clip = TextClip::from_min_size(0.0, 0.0, 360.0, args.size_px * 1.4).unwrap();
        let clipped_glyphs = engine
            .text_clipped(0.0, baseline_y, text, &args, clip)
            .len();

        assert!(clipped_glyphs > 0);
        assert!(clipped_glyphs < all_glyphs);
        assert_eq!(engine.flush().len(), clipped_glyphs * 6);
    }
}
