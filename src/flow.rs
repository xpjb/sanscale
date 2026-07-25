//! Line flow (Level 2): break a shaped paragraph into lines and attach caret
//! stops. Everything here is em-space and pixel-free, which is what makes the
//! layout cache zoom-invariant.
//!
//! Greedy first-fit over already-shaped advances — no reshaping, so a reflow is
//! cheap. A future optimal-breaking pass swaps this step out without touching
//! anything above or below it.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::layout::ShapedGlyph;

/// One caret position: a byte offset and where it sits along the line, in em.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaretStop {
    pub byte_index: usize,
    pub x_em: f32,
}

/// One laid-out line of a paragraph. Glyph `x` is relative to the line's own
/// origin, so alignment is a per-line offset applied at draw.
#[derive(Clone, Debug)]
pub(crate) struct FlowLine {
    pub source: Range<usize>,
    pub glyphs: Vec<ShapedGlyph>,
    /// Total advance of the line, em.
    pub advance: f32,
    pub carets: Vec<CaretStop>,
}

/// Break `paragraph` into lines at `max_width_em` (`f32::MAX` for no wrapping)
/// and attach caret stops. Byte offsets are paragraph-local; the block assembles
/// them into document-global ones.
pub(crate) fn flow_paragraph(
    paragraph: &str,
    glyphs: &[ShapedGlyph],
    max_width_em: f32,
) -> Vec<FlowLine> {
    let mut lines = Vec::new();
    wrap(paragraph, glyphs, max_width_em, &mut lines);
    attach_carets(&mut lines, paragraph);
    lines
}

fn wrap(paragraph: &str, glyphs: &[ShapedGlyph], max_width_em: f32, out: &mut Vec<FlowLine>) {
    if paragraph.is_empty() {
        out.push(FlowLine {
            source: 0..0,
            glyphs: Vec::new(),
            advance: 0.0,
            carets: Vec::new(),
        });
        return;
    }

    let boundaries = grapheme_boundaries(paragraph);
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

        // A single token wider than the line gets broken mid-word.
        if token_width > max_width_em && token_glyphs.len() > 1 {
            if let Some(start) = current_start.take() {
                flush(start, current_end, current_origin, current_width, &mut current_glyphs, out);
            }
            flush_broken_token(
                token_start,
                token_end,
                token_origin,
                token_glyphs,
                max_width_em,
                &boundaries,
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

        flush(
            current_start.take().unwrap(),
            current_end,
            current_origin,
            current_width,
            &mut current_glyphs,
            out,
        );

        // Trailing whitespace at a break hangs off the end rather than opening
        // the next line with a leading space.
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
        flush(start, current_end, current_origin, current_width, &mut current_glyphs, out);
    }
}

fn flush_broken_token(
    token_start: usize,
    token_end: usize,
    token_origin: f32,
    token_glyphs: Vec<ShapedGlyph>,
    max_width_em: f32,
    boundaries: &[usize],
    out: &mut Vec<FlowLine>,
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
            let break_byte = grapheme_floor(boundaries, glyph.cluster);
            // Never break before the line's own start — that would loop.
            if break_byte <= line_start {
                line_width = proposed_width;
                line_end = token_end;
                line_glyphs.push(glyph);
                continue;
            }
            flush(line_start, break_byte, line_origin, line_width, &mut line_glyphs, out);
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
        flush(line_start, line_end, line_origin, line_width, &mut line_glyphs, out);
    }
}

fn flush(
    start: usize,
    end: usize,
    origin: f32,
    width: f32,
    glyphs: &mut Vec<ShapedGlyph>,
    out: &mut Vec<FlowLine>,
) {
    let line_glyphs = glyphs
        .drain(..)
        .map(|mut glyph| {
            glyph.x -= origin;
            glyph
        })
        .collect();
    out.push(FlowLine {
        source: start..end,
        glyphs: line_glyphs,
        advance: width.max(0.0),
        carets: Vec::new(),
    });
}

fn attach_carets(lines: &mut [FlowLine], paragraph: &str) {
    let boundaries = grapheme_boundaries(paragraph);

    for line in lines {
        let mut carets = vec![CaretStop {
            byte_index: line.source.start,
            x_em: 0.0,
        }];
        for glyph in &line.glyphs {
            let glyph_start = grapheme_floor(&boundaries, glyph.cluster);
            if carets
                .last()
                .is_none_or(|caret| caret.byte_index != glyph_start)
            {
                carets.push(CaretStop {
                    byte_index: glyph_start,
                    x_em: glyph.x,
                });
            }
            let after = next_grapheme_boundary(&boundaries, glyph.cluster);
            if after <= line.source.end
                && carets.last().is_none_or(|caret| caret.byte_index != after)
            {
                carets.push(CaretStop {
                    byte_index: after,
                    x_em: glyph.x + glyph.advance_x,
                });
            }
        }
        carets.push(CaretStop {
            byte_index: line.source.end,
            x_em: line.advance,
        });
        carets.sort_by_key(|caret| caret.byte_index);
        carets.dedup_by_key(|caret| caret.byte_index);
        line.carets = carets;
    }
}

/// Split into alternating whitespace / non-whitespace tokens — the break
/// opportunities for greedy first-fit.
fn tokenize(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0usize;
    std::iter::from_fn(move || {
        if start >= text.len() {
            return None;
        }
        let first = text[start..].chars().next()?;
        let ws = first.is_whitespace();
        let mut end = start;
        for (offset, ch) in text[start..].char_indices() {
            if ch.is_whitespace() != ws {
                break;
            }
            end = start + offset + ch.len_utf8();
        }
        let token = &text[start..end];
        let token_start = start;
        start = end;
        Some((token_start, token))
    })
}

fn grapheme_boundaries(content: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = content.grapheme_indices(true).map(|(i, _)| i).collect();
    if boundaries.first().copied() != Some(0) {
        boundaries.insert(0, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(cluster: usize, x: f32, advance: f32) -> ShapedGlyph {
        ShapedGlyph {
            glyph_id: 1,
            font_id: 0,
            is_color: false,
            cluster,
            x,
            y: 0.0,
            advance_x: advance,
            info: None,
        }
    }

    #[test]
    fn tokenize_alternates_and_tiles_the_input() {
        let text = "ab  cd ";
        let tokens: Vec<_> = tokenize(text).collect();
        assert_eq!(tokens, vec![(0, "ab"), (2, "  "), (4, "cd"), (6, " ")]);
        // Contiguous and complete.
        let mut at = 0;
        for (start, token) in &tokens {
            assert_eq!(*start, at);
            at += token.len();
        }
        assert_eq!(at, text.len());
    }

    #[test]
    fn unwrapped_paragraph_is_one_line() {
        let glyphs: Vec<_> = (0..3).map(|i| glyph(i, i as f32, 1.0)).collect();
        let lines = flow_paragraph("abc", &glyphs, f32::MAX);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].source, 0..3);
        assert!((lines[0].advance - 3.0).abs() < 1e-6);
    }

    #[test]
    fn wraps_at_a_space_and_drops_the_break_whitespace() {
        // "ab cd", each glyph 1em wide, wrapping at 3em.
        let glyphs: Vec<_> = (0..5).map(|i| glyph(i, i as f32, 1.0)).collect();
        let lines = flow_paragraph("ab cd", &glyphs, 3.0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].source.start, 0);
        assert_eq!(lines[1].source, 3..5);
        // The second line's glyphs are rebased to its own origin.
        assert!((lines[1].glyphs[0].x - 0.0).abs() < 1e-6);
    }

    #[test]
    fn a_single_overlong_token_breaks_mid_word() {
        let glyphs: Vec<_> = (0..6).map(|i| glyph(i, i as f32, 1.0)).collect();
        let lines = flow_paragraph("abcdef", &glyphs, 2.0);
        assert!(lines.len() > 1, "an unbreakable token must still be split");
        // Lines tile the input with no gaps or overlaps.
        assert_eq!(lines[0].source.start, 0);
        for pair in lines.windows(2) {
            assert_eq!(pair[0].source.end, pair[1].source.start);
        }
        assert_eq!(lines.last().unwrap().source.end, 6);
    }

    #[test]
    fn empty_paragraph_still_yields_one_caret_line() {
        let lines = flow_paragraph("", &[], 10.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].carets.len(), 1);
        assert_eq!(lines[0].carets[0].byte_index, 0);
    }

    #[test]
    fn carets_cover_every_grapheme_boundary_in_order() {
        let glyphs: Vec<_> = (0..3).map(|i| glyph(i, i as f32, 1.0)).collect();
        let lines = flow_paragraph("abc", &glyphs, f32::MAX);
        let stops: Vec<_> = lines[0].carets.iter().map(|c| c.byte_index).collect();
        assert_eq!(stops, vec![0, 1, 2, 3]);
        // Monotonic in x, and the last stop is the line's advance.
        assert!(lines[0].carets.windows(2).all(|w| w[0].x_em <= w[1].x_em));
        assert!((lines[0].carets.last().unwrap().x_em - lines[0].advance).abs() < 1e-6);
    }
}
