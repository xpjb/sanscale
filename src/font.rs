//! Font loading: keeps font bytes alive for a `'static` rustybuzz/ttf-parser face,
//! exposes shaping, outline extraction, and per-glyph metrics.

use crate::outline::{GlyphOutlines, OutlineCollector};
use rustybuzz::Face as RustyFace;
use ttf_parser::GlyphId;

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

/// A loaded font. Bytes are heap-leaked so the underlying face has a `'static` lifetime.
pub(crate) struct Font {
    #[allow(dead_code)]
    data: &'static [u8],
    face: RustyFace<'static>,
    units_per_em: u16,
    metrics: FontMetrics,
}

impl Font {
    /// Load from a file path.
    pub fn load(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
        Self::from_bytes(bytes)
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        Self::from_bytes_with_index(bytes, 0)
    }

    pub fn from_bytes_with_index(bytes: Vec<u8>, face_index: u32) -> Result<Self, String> {
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let face = RustyFace::from_slice(leaked, face_index)
            .ok_or_else(|| "failed to parse font".to_string())?;
        let units_per_em = face.units_per_em() as u16;
        let upem = units_per_em as f32;
        let metrics = FontMetrics {
            ascent: face.ascender() as f32 / upem,
            descent: face.descender() as f32 / upem,
            line_gap: face.line_gap() as f32 / upem,
        };
        Ok(Self {
            data: leaked,
            face,
            units_per_em,
            metrics,
        })
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
    /// itemization — the first face in a [`FontSet`] that covers a character wins.
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

/// Where a font's bytes come from when building a [`FontSet`]. `Bytes` carries the
/// face index within a collection (0 for a single font) so `.ttc` faces resolved
/// by name (fontdb) or bundled via `include_bytes!` pick the right face.
pub enum FontSource<'a> {
    Bytes(Vec<u8>, u32),
    Path(&'a str),
}

/// An ordered fallback chain: `faces[0]` is primary, the rest are fallbacks tried
/// in order for characters the primary lacks. The primary defines line metrics.
pub(crate) struct FontSet {
    faces: Vec<Font>,
}

impl FontSet {
    /// Load an ordered set of sources. Sources that fail to load are skipped; the
    /// result errors only if *none* load (so a missing system fallback is fine, a
    /// totally empty chain is not).
    pub fn load(sources: Vec<FontSource<'_>>) -> Result<Self, String> {
        let mut faces = Vec::with_capacity(sources.len());
        for source in sources {
            let loaded = match source {
                FontSource::Bytes(bytes, index) => Font::from_bytes_with_index(bytes, index),
                FontSource::Path(path) => Font::load(path),
            };
            match loaded {
                Ok(font) => faces.push(font),
                Err(err) => eprintln!("font fallback skipped: {err}"),
            }
        }
        if faces.is_empty() {
            return Err("FontSet: no fonts loaded".to_string());
        }
        Ok(Self { faces })
    }

    pub fn primary(&self) -> &Font {
        &self.faces[0]
    }

    /// The first face in the chain with a glyph for `c`, or `None` if no face
    /// covers it (would render as `.notdef` tofu). Diagnostic helper.
    pub fn covering_face(&self, c: char) -> Option<u16> {
        self.faces
            .iter()
            .position(|f| f.has_glyph(c))
            .map(|i| i as u16)
    }

    /// First face that renders `c` as a color (emoji) glyph — used to honor an
    /// emoji variation selector (U+FE0F).
    pub fn color_face_for(&self, c: char) -> Option<u16> {
        self.faces
            .iter()
            .position(|f| f.color_glyph(c))
            .map(|i| i as u16)
    }

    /// First face that covers `c` with a monochrome glyph — used to honor a text
    /// variation selector (U+FE0E).
    pub fn text_face_for(&self, c: char) -> Option<u16> {
        self.faces
            .iter()
            .position(|f| f.has_glyph(c) && !f.color_glyph(c))
            .map(|i| i as u16)
    }

    /// Family names of every face in the chain, in fallback order.
    pub fn family_names(&self) -> Vec<String> {
        self.faces
            .iter()
            .map(|f| f.family_name().unwrap_or_else(|| "<unknown>".to_string()))
            .collect()
    }

    /// Face by id; ids are indices produced by itemization, always in range.
    pub fn face(&self, id: u16) -> &Font {
        &self.faces[id as usize]
    }

    pub fn faces(&self) -> &[Font] {
        &self.faces
    }
}
