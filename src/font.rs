//! Font loading. Holds the shared bytes alive beside the parsed face.

use std::sync::Arc;

use rustybuzz::Face as RustyFace;
use ttf_parser::GlyphId;

use crate::outline::{GlyphOutlines, OutlineCollector};
use crate::text::{FontData, FontError};

/// Vertical font metrics, in em-space.
#[derive(Clone, Copy, Debug)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}

impl FontMetrics {
    /// Default line-to-line advance: ascent - descent + line_gap (descent is negative).
    pub fn line_height(&self) -> f32 {
        self.ascent - self.descent + self.line_gap
    }
}

/// A loaded font: the shared bytes, plus a face borrowing them.
///
/// The face is `'static` only because it borrows through the `Arc` this struct
/// owns. That is why `data` is kept and why the two are never separated — see the
/// safety note in [`Font::from_shared`]. The previous version `Box::leak`ed the
/// bytes instead, which made a font impossible to free.
pub(crate) struct Font {
    /// Keeps the bytes alive for `face`. Never handed out; never dropped early.
    data: FontData,
    face: RustyFace<'static>,
    units_per_em: u16,
    metrics: FontMetrics,
}

impl Font {
    pub fn from_shared(data: FontData, face_index: u32) -> Result<Self, FontError> {
        // SAFETY: `bytes` points into the allocation owned by `data`, which this
        // struct holds for its whole life and never reallocates (an `Arc`'s payload
        // is pinned in place regardless of the `Font` moving). `face` is private
        // and never escapes this struct, so no borrow can outlive `data`. `data` is
        // declared before `face`, so drop order tears the face down first.
        let bytes: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>((*data).as_ref()) };
        let face = RustyFace::from_slice(bytes, face_index).ok_or(FontError::Parse)?;
        let units_per_em = face.units_per_em() as u16;
        let upem = units_per_em as f32;
        let metrics = FontMetrics {
            ascent: face.ascender() as f32 / upem,
            descent: face.descender() as f32 / upem,
            line_gap: face.line_gap() as f32 / upem,
        };
        Ok(Self {
            data,
            face,
            units_per_em,
            metrics,
        })
    }

    /// Identity of the underlying bytes, for deduping. Two chains that were handed
    /// the same `Arc` map to one entry — which is what lets a shared emoji font be
    /// rasterized once instead of once per chain.
    pub fn data_identity(&self) -> (*const u8, usize, u16) {
        let bytes: &[u8] = (*self.data).as_ref();
        (bytes.as_ptr(), bytes.len(), self.units_per_em)
    }

    pub fn face(&self) -> &RustyFace<'static> {
        &self.face
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    /// True when this face has a glyph for `c` (cmap coverage). Drives fallback
    /// itemization — the first face in a chain that covers a character wins.
    pub fn has_glyph(&self, c: char) -> bool {
        self.face.glyph_index(c).is_some()
    }

    /// True when `glyph_id` is a `COLR` color glyph (emoji) rather than a
    /// monochrome outline — such glyphs route to the emoji atlas, not Slug.
    pub fn is_color_glyph(&self, glyph_id: u16) -> bool {
        self.face.is_color_glyph(GlyphId(glyph_id))
    }

    /// True when this face covers `c` *and* its glyph is a color (COLR) glyph —
    /// i.e. this face would render `c` as emoji.
    pub fn color_glyph(&self, c: char) -> bool {
        self.face
            .glyph_index(c)
            .is_some_and(|g| self.face.is_color_glyph(g))
    }

    /// Human-readable family name (from the `name` table), for diagnostics.
    pub fn family_name(&self) -> Option<String> {
        self.face
            .names()
            .into_iter()
            .find(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
            .and_then(|n| n.to_string())
    }

    /// Extract quadratic Bézier outlines for a glyph in em-space.
    pub fn load_glyph(&self, glyph_id: GlyphId) -> Option<GlyphOutlines> {
        let mut builder = OutlineCollector::new(self.units_per_em);
        self.face
            .outline_glyph(glyph_id, &mut builder)
            .map(|_| builder.finish())
    }
}

/// Read a font file into shared bytes. Convenience for examples and tests; a real
/// consumer discovers fonts itself and hands over an `Arc` it already has.
pub fn read_font_file(path: &str) -> std::io::Result<FontData> {
    Ok(Arc::new(std::fs::read(path)?))
}
