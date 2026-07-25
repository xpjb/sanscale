//! Text shaping. Itemizes text into per-face runs (font fallback), shapes each
//! run with rustybuzz, and flattens to a `ShapedRun` in em-space.
//!
//! Fallback resolution walks a *chain* — an ordered borrow of fonts out of the
//! service's global pool. Glyphs carry the **global** font id, not the position
//! within the chain, so two chains sharing a face (typically the emoji font) key
//! into one glyph cache entry instead of two.

use rustybuzz::{shape, UnicodeBuffer};
use ttf_parser::GlyphId;
use unicode_segmentation::UnicodeSegmentation;

use crate::bands::{process_bands_with, BandsScratch};
use crate::cache::{GlyphCache, GlyphInfo};
use crate::font::Font;

/// One font of a chain: its global pool id plus a borrow of the font itself.
#[derive(Clone, Copy)]
pub(crate) struct ChainFont<'a> {
    pub id: u16,
    pub font: &'a Font,
}

/// One positioned glyph along a baseline (`x`/`y` are em-space pen positions).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShapedGlyph {
    pub glyph_id: u32,
    /// Global font-pool id, so the glyph cache is shared across chains.
    pub font_id: u16,
    /// Color glyph (`COLR`/`CBDT`/`sbix`): rendered via the emoji atlas, not
    /// Slug — `info` is `None` for these.
    pub is_color: bool,
    pub cluster: usize,
    pub x: f32,
    pub y: f32,
    pub advance_x: f32,
    pub info: Option<GlyphInfo>,
}

/// A shaped run of glyphs along a single baseline.
#[derive(Clone, Debug, Default)]
pub(crate) struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
}

/// Emoji (VS16) and text (VS15) variation selectors, which override the
/// presentation of the preceding base character.
const VS16: char = '\u{FE0F}';
const VS15: char = '\u{FE0E}';

/// A Fitzpatrick skin-tone modifier (`Emoji_Modifier`). Its presence forces the
/// whole sequence to emoji presentation regardless of the base's default — so
/// e.g. `✌\u{1F3FF}` (a text-default base) still resolves to the color face.
fn is_emoji_modifier(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// A multi-code-point emoji sequence — ZWJ, flag (regional indicators), skin
/// tone, or keycap — for which cluster-level fallback is worthwhile.
fn is_emoji_sequence(grapheme: &str) -> bool {
    grapheme.chars().nth(1).is_some()
        && grapheme.chars().any(|c| {
            c == '\u{200D}' // ZWJ
                || c == VS16
                || is_emoji_modifier(c)
                || ('\u{1F1E6}'..='\u{1F1FF}').contains(&c) // regional indicator
                || c == '\u{20E3}' // combining enclosing keycap
        })
}

/// Whether `font` shapes `text` to a single **color** glyph — i.e. it can form
/// the whole emoji sequence (a flag, a ZWJ emoji) into one color glyph rather
/// than several pieces. Requiring color avoids a monochrome text font hijacking
/// a non-ligating sequence (which would render a mono base instead of a tofu).
fn shapes_single_color(font: &Font, text: &str) -> bool {
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let glyphs = shape(font.face(), &[], buffer);
    if glyphs.len() != 1 {
        return false;
    }
    let gid = glyphs.glyph_infos()[0].glyph_id;
    gid != 0 && font.is_color_glyph(gid as u16)
}

/// Whether `font` shapes `text` to exactly one glyph, of any kind. Used to ask
/// what a *already-chosen* face will do; choosing the face is
/// [`face_for_grapheme`]'s job, and its emoji-sequence arm requires *color*
/// (see `shapes_single_color`).
pub(crate) fn shapes_to_single_glyph(font: &Font, text: &str) -> bool {
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let glyphs = shape(font.face(), &[], buffer);
    glyphs.len() == 1 && glyphs.glyph_infos()[0].glyph_id != 0
}

/// First chain position whose font ligates the whole grapheme into one color glyph.
fn single_glyph_face(chain: &[ChainFont<'_>], grapheme: &str) -> Option<usize> {
    chain
        .iter()
        .position(|entry| shapes_single_color(entry.font, grapheme))
}

/// First chain position covering `c`, or `None` if nothing does (→ tofu).
fn covering_face(chain: &[ChainFont<'_>], c: char) -> Option<usize> {
    chain.iter().position(|entry| entry.font.has_glyph(c))
}

/// First chain position rendering `c` as a color glyph — honors U+FE0F.
fn color_face_for(chain: &[ChainFont<'_>], c: char) -> Option<usize> {
    chain.iter().position(|entry| entry.font.color_glyph(c))
}

/// First chain position covering `c` with a monochrome glyph — honors U+FE0E.
fn text_face_for(chain: &[ChainFont<'_>], c: char) -> Option<usize> {
    chain
        .iter()
        .position(|entry| entry.font.has_glyph(c) && !entry.font.color_glyph(c))
}

/// The chain position a grapheme resolves to. Primary (0) for whitespace/control
/// (so runs don't fragment on spaces) and for characters no face covers (→
/// `.notdef` tofu).
///
/// Presentation follows Unicode: a trailing U+FE0F forces emoji (color) and
/// U+FE0E forces text; with no selector, the base character's
/// `Emoji_Presentation` property decides (so `😀` is color and `❤` is text by
/// default, while `❤\u{FE0F}` is the red heart). The chosen presentation prefers
/// a color or monochrome face accordingly, falling back to any covering face.
pub(crate) fn face_for_grapheme(chain: &[ChainFont<'_>], grapheme: &str) -> usize {
    let base = grapheme.chars().next().unwrap_or(' ');
    if base.is_whitespace() || base.is_control() {
        return 0;
    }
    let want_color = if grapheme.chars().any(|c| c == VS16 || is_emoji_modifier(c)) {
        true
    } else if grapheme.chars().any(|c| c == VS15) {
        false
    } else {
        crate::emoji_presentation::is_default_emoji(base)
    };
    let preferred = if want_color {
        color_face_for(chain, base)
    } else {
        text_face_for(chain, base)
    }
    .or_else(|| covering_face(chain, base));

    // Cluster-level fallback: for an emoji sequence, if the presentation-preferred
    // face can't render it as a single glyph (e.g. Segoe UI Emoji has the regional
    // indicators but no flag ligatures), fall back to a face in the chain that can.
    if is_emoji_sequence(grapheme) {
        if let Some(p) = preferred {
            if shapes_single_color(chain[p].font, grapheme) {
                return p;
            }
        }
        if let Some(f) = single_glyph_face(chain, grapheme) {
            return f;
        }
    }
    preferred.unwrap_or(0)
}

/// Whether `grapheme` renders as exactly one glyph *through the real fallback
/// walk* — the face [`face_for_grapheme`] picks, not any face that happens to
/// ligate it.
///
/// The distinction is load-bearing for grid layouts: asking "does *some* face
/// shape this to one glyph" accepts a monochrome ligation the walk would reject
/// (its emoji-sequence arm requires color), so a caller that draws the sequence
/// on that answer gets overlapping pieces where it expected one cell.
pub(crate) fn resolves_to_single_glyph(chain: &[ChainFont<'_>], grapheme: &str) -> bool {
    let Some(entry) = chain.get(face_for_grapheme(chain, grapheme)) else {
        return false;
    };
    shapes_to_single_glyph(entry.font, grapheme)
}

/// Split `text` into maximal `(byte_start, run_text, chain_position)` runs where
/// every grapheme resolves to the same face. Grapheme-based so combining
/// sequences stay with their base in one face.
fn itemize<'a>(chain: &[ChainFont<'_>], text: &'a str) -> Vec<(usize, &'a str, usize)> {
    let mut runs = Vec::new();
    let mut run_start = 0usize;
    let mut run_face: Option<usize> = None;

    for (idx, grapheme) in text.grapheme_indices(true) {
        let face = face_for_grapheme(chain, grapheme);
        match run_face {
            Some(f) if f == face => {}
            Some(f) => {
                runs.push((run_start, &text[run_start..idx], f));
                run_start = idx;
                run_face = Some(face);
            }
            None => run_face = Some(face),
        }
    }
    if let Some(f) = run_face {
        runs.push((run_start, &text[run_start..], f));
    }
    runs
}

/// Shape `text` across the fallback chain, ensuring every non-color glyph is
/// populated in `cache` (keyed by `(font_id, glyph_id)`). Pen positions are
/// em-space starting at the origin; clusters are byte offsets into `text`.
pub(crate) fn shape_text(
    chain: &[ChainFont<'_>],
    cache: &mut GlyphCache,
    text: &str,
) -> ShapedRun {
    if chain.is_empty() {
        return ShapedRun::default();
    }
    let mut glyphs = Vec::new();
    let mut x = 0.0f32;
    let mut y = 0.0f32;
    let mut scratch = BandsScratch::default();

    for (run_start, run_text, position) in itemize(chain, text) {
        let entry = chain[position];
        let font = entry.font;
        let upem = font.units_per_em() as f32;

        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(run_text);
        buffer.guess_segment_properties();
        let glyph_buffer = shape(font.face(), &[], buffer);

        for (info, pos) in glyph_buffer
            .glyph_infos()
            .iter()
            .zip(glyph_buffer.glyph_positions().iter())
        {
            let glyph_id = info.glyph_id;
            let gx = x + pos.x_offset as f32 / upem;
            let gy = y + pos.y_offset as f32 / upem;
            let x_adv = pos.x_advance as f32 / upem;
            let y_adv = pos.y_advance as f32 / upem;

            // Color glyphs render through the emoji atlas, so skip Slug band
            // generation (`info` stays `None`); the emission step routes them.
            let is_color = font.is_color_glyph(glyph_id as u16);
            let cached = if is_color {
                None
            } else {
                cache.get((entry.id, glyph_id)).or_else(|| {
                    let outlines = font.load_glyph(GlyphId(glyph_id as u16))?;
                    let band = process_bands_with(&outlines, &mut scratch);
                    Some(cache.insert((entry.id, glyph_id), band))
                })
            };

            glyphs.push(ShapedGlyph {
                glyph_id,
                font_id: entry.id,
                is_color,
                cluster: run_start + info.cluster as usize,
                x: gx,
                y: gy,
                advance_x: x_adv,
                info: cached,
            });

            x += x_adv;
            y += y_adv;
        }
    }

    ShapedRun { glyphs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::read_font_file;

    fn exists(p: &str) -> bool {
        std::path::Path::new(p).exists()
    }
    fn latin_path() -> Option<&'static str> {
        ["C:\\Windows\\Fonts\\segoeui.ttf", "C:\\Windows\\Fonts\\arial.ttf"]
            .into_iter()
            .find(|p| exists(p))
    }
    fn cjk_path() -> Option<&'static str> {
        [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\simsun.ttc",
            "C:\\Windows\\Fonts\\malgun.ttf",
            "C:\\Windows\\Fonts\\YuGothM.ttc",
        ]
        .into_iter()
        .find(|p| exists(p))
    }

    fn load(paths: &[&str]) -> Vec<Font> {
        paths
            .iter()
            .filter_map(|p| Font::from_shared(read_font_file(p).ok()?, 0).ok())
            .collect()
    }

    fn chain(fonts: &[Font]) -> Vec<ChainFont<'_>> {
        fonts
            .iter()
            .enumerate()
            .map(|(i, font)| ChainFont {
                id: i as u16,
                font,
            })
            .collect()
    }

    #[test]
    fn single_font_is_one_run() {
        let Some(latin) = latin_path() else { return };
        let fonts = load(&[latin]);
        if fonts.is_empty() {
            return;
        }
        assert_eq!(itemize(&chain(&fonts), "hello world 123").len(), 1);
    }

    #[test]
    fn fallback_resolves_cjk_without_tofu() {
        let (Some(latin), Some(cjk)) = (latin_path(), cjk_path()) else {
            return;
        };
        let fonts = load(&[latin, cjk]);
        if fonts.len() < 2 {
            return;
        }
        let chain = chain(&fonts);
        let mut cache = GlyphCache::new();
        let text = "A日";
        let run = shape_text(&chain, &mut cache, text);

        // Every glyph is real (no `.notdef` tofu) and band-cached.
        assert!(run.glyphs.iter().all(|g| g.glyph_id != 0));
        assert!(run.glyphs.iter().all(|g| g.info.is_some()));
        // The CJK ideograph resolved to the fallback face, not the Latin primary.
        assert!(
            run.glyphs.iter().any(|g| g.font_id == 1),
            "日 should resolve to the CJK fallback face"
        );
        // Clusters stay valid byte offsets into the source.
        for g in &run.glyphs {
            assert!(text.is_char_boundary(g.cluster));
        }
    }

    #[test]
    fn variation_selector_forces_emoji_presentation() {
        let sym = "C:\\Windows\\Fonts\\seguisym.ttf";
        let emoji = "C:\\Windows\\Fonts\\seguiemj.ttf";
        if !exists(sym) || !exists(emoji) {
            return;
        }
        let fonts = load(&[sym, emoji]);
        if fonts.len() < 2 {
            return;
        }
        let chain = chain(&fonts);
        let mut cache = GlyphCache::new();

        // A monochrome symbol font precedes the emoji font, so a bare heart is
        // text (its first covering face is the mono one)…
        let mono = shape_text(&chain, &mut cache, "\u{2764}");
        assert!(
            mono.glyphs.iter().all(|g| !g.is_color),
            "bare U+2764 should render as text"
        );
        // …but U+2764 U+FE0F forces the color face despite the ordering.
        let color = shape_text(&chain, &mut cache, "\u{2764}\u{FE0F}");
        assert!(
            color.glyphs.iter().any(|g| g.is_color),
            "U+2764 U+FE0F should render as a color glyph"
        );
    }

    #[test]
    fn skin_tone_modifier_forces_emoji_presentation() {
        let sym = "C:\\Windows\\Fonts\\seguisym.ttf";
        let emoji = "C:\\Windows\\Fonts\\seguiemj.ttf";
        if !exists(sym) || !exists(emoji) {
            return;
        }
        let fonts = load(&[sym, emoji]);
        if fonts.len() < 2 {
            return;
        }
        let chain = chain(&fonts);
        let mut cache = GlyphCache::new();
        // Victory hand (text-default base) + a skin-tone modifier must resolve to
        // the color face — otherwise the modifier lands on a mono font as tofu.
        let run = shape_text(&chain, &mut cache, "\u{270C}\u{1F3FF}");
        assert!(
            run.glyphs.iter().any(|g| g.is_color),
            "U+270C U+1F3FF should render as a color glyph"
        );
        assert!(
            run.glyphs.iter().all(|g| g.glyph_id != 0),
            "the skin-tone modifier should not fall back to .notdef tofu"
        );
    }

    #[test]
    fn itemize_splits_latin_and_cjk_runs() {
        let (Some(latin), Some(cjk)) = (latin_path(), cjk_path()) else {
            return;
        };
        let fonts = load(&[latin, cjk]);
        if fonts.len() < 2 {
            return;
        }
        let runs = itemize(&chain(&fonts), "Hi 日本");
        // "Hi " on the primary, "日本" on the fallback.
        assert!(runs.len() >= 2);
        assert_eq!(runs[0].2, 0);
        assert!(runs.iter().any(|r| r.2 == 1));
        // Runs tile the input contiguously and on char boundaries.
        assert_eq!(runs[0].0, 0);
        for pair in runs.windows(2) {
            assert_eq!(pair[0].0 + pair[0].1.len(), pair[1].0);
        }
    }
}
