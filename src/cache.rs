//! Glyph cache: combines per-glyph band data into shared GPU textures.

use std::collections::HashMap;

use crate::bands::{BandData, BAND_TEXTURE_WIDTH, CURVE_TEXTURE_WIDTH};

/// Cache key: `(face_id, glyph_id)`. `glyph_id` alone is ambiguous across a
/// fallback chain — glyph 5 in Noto Sans is not glyph 5 in an emoji font — so the
/// face index from itemization is part of the key. The shared curve/band atlas is
/// unaffected; only which `GlyphInfo` a key maps to changes.
pub(crate) type GlyphKey = (u16, u32);

#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphInfo {
    pub band_start: (u32, u32),
    pub band_max: (u32, u32),
    pub bbox: (f32, f32, f32, f32),
}

pub(crate) struct GlyphCache {
    curve_texels: Vec<[u16; 4]>,
    band_texels: Vec<[u16; 2]>,
    curve_width: u32,
    curve_height: u32,
    band_width: u32,
    band_height: u32,
    glyphs: HashMap<GlyphKey, GlyphInfo>,
    revision: u64,
}

impl GlyphCache {
    pub fn new() -> Self {
        Self {
            curve_texels: Vec::new(),
            band_texels: Vec::new(),
            curve_width: CURVE_TEXTURE_WIDTH,
            curve_height: 0,
            band_width: BAND_TEXTURE_WIDTH,
            band_height: 0,
            glyphs: HashMap::new(),
            revision: 0,
        }
    }

    pub fn get(&self, key: GlyphKey) -> Option<GlyphInfo> {
        self.glyphs.get(&key).copied()
    }

    /// Insert a glyph's processed band data; returns its assigned `GlyphInfo`.
    pub fn insert(&mut self, key: GlyphKey, band_data: BandData) -> GlyphInfo {
        let header_count = (band_data.band_max.0 + 1 + band_data.band_max.1 + 1) as usize;
        let curve_start = self.alloc_curves(&band_data.curve_texels);
        let band_start = self.alloc_bands(&band_data.band_texels, header_count, curve_start);
        let info = GlyphInfo {
            band_start,
            band_max: band_data.band_max,
            bbox: band_data.bbox,
        };
        self.glyphs.insert(key, info);
        self.revision = self.revision.wrapping_add(1);
        info
    }

    fn alloc_curves(&mut self, texels: &[[u16; 4]]) -> (u32, u32) {
        let w = CURVE_TEXTURE_WIDTH as usize;
        let start = self.curve_texels.len();
        let col = (start % w) as u32;
        let row = (start / w) as u32;
        self.curve_texels.extend_from_slice(texels);
        let end = self.curve_texels.len();
        let end_row = end.div_ceil(w);
        self.curve_height = self.curve_height.max(end_row as u32);
        (col, row)
    }

    /// Copy one glyph's band block into the shared atlas, upholding Slug's layout
    /// invariant: the header block and every band's curve-loc list must each stay
    /// within a single texture row. The shader indexes both without row-wrapping
    /// (only the header→list jump wraps, via `calc_band_loc`, and only curve-texel
    /// reads wrap, in `fetch_curve`), so a run that straddles the 4096-texel
    /// boundary would read the wrong row — a missing band or scrambled glyph. We
    /// reproduce the padding Slug's font compiler performs: skip to the next row
    /// whenever a run would cross the boundary. The offsets `bands.rs` packed are
    /// recomputed here, since padding shifts each list off its packed position.
    fn alloc_bands(
        &mut self,
        texels: &[[u16; 2]],
        header_count: usize,
        curve_start: (u32, u32),
    ) -> (u32, u32) {
        let w = BAND_TEXTURE_WIDTH as usize;

        // The header block is read on one row starting at the glyph origin, so
        // advance the origin to the next row if the headers would cross a boundary.
        if self.band_texels.len() % w + header_count > w {
            self.pad_bands_to_row(w);
        }
        let origin = self.band_texels.len();
        let col = (origin % w) as u32;
        let row = (origin / w) as u32;

        // Reserve the header texels; each is patched once its list is placed.
        self.band_texels
            .resize(origin + header_count, [0, 0]);

        for b in 0..header_count {
            let count = texels[b][0] as usize;
            let src_off = texels[b][1] as usize;

            // Keep this band's list within one row.
            if count > 0 && self.band_texels.len() % w + count > w {
                self.pad_bands_to_row(w);
            }
            let offset = (self.band_texels.len() - origin) as u16;
            self.band_texels[origin + b] = [count as u16, offset];

            for t in &texels[src_off..src_off + count] {
                let abs = offset_curve_coord(curve_start, (t[0] as u32, t[1] as u32));
                self.band_texels.push([abs.0 as u16, abs.1 as u16]);
            }
        }

        let end = self.band_texels.len();
        self.band_height = self.band_height.max(end.div_ceil(w) as u32);
        (col, row)
    }

    /// Pad the band buffer up to the start of the next texture row with dead
    /// (never-read) texels, so the next run begins row-aligned.
    fn pad_bands_to_row(&mut self, w: usize) {
        let rem = self.band_texels.len() % w;
        if rem != 0 {
            self.band_texels
                .resize(self.band_texels.len() + (w - rem), [0, 0]);
        }
    }

    pub fn curve_data(&self) -> &[[u16; 4]] {
        &self.curve_texels
    }
    pub fn band_data(&self) -> &[[u16; 2]] {
        &self.band_texels
    }
    pub fn curve_size(&self) -> (u32, u32) {
        (self.curve_width, self.curve_height.max(1))
    }
    pub fn band_size(&self) -> (u32, u32) {
        (self.band_width, self.band_height.max(1))
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

fn offset_curve_coord(start: (u32, u32), local: (u32, u32)) -> (u32, u32) {
    let w = CURVE_TEXTURE_WIDTH as usize;
    let absolute =
        start.1 as usize * w + start.0 as usize + local.1 as usize * w + local.0 as usize;
    ((absolute % w) as u32, (absolute / w) as u32)
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic glyph with `num_h` + `num_v` bands, each listing `per_band`
    /// dummy curve-locs. Mirrors the header-then-locs layout `bands.rs` emits.
    fn synthetic(num_h: u32, num_v: u32, per_band: usize) -> BandData {
        let header_count = (num_h + num_v) as usize;
        let mut band_texels = vec![[0u16; 2]; header_count];
        let mut running = header_count as u16;
        for b in 0..header_count {
            band_texels[b] = [per_band as u16, running];
            for k in 0..per_band {
                band_texels.push([(k as u16) & 0x0f, 0]);
            }
            running += per_band as u16;
        }
        BandData {
            curve_texels: vec![[0; 4]; 3],
            band_texels,
            band_max: (num_v - 1, num_h - 1),
            bbox: (0.0, 0.0, 1.0, 1.0),
        }
    }

    /// The shader reads the header block and each band's curve-loc list without
    /// row-wrapping, so neither may straddle a 4096-texel row boundary. Fill the
    /// atlas across many boundaries and assert the invariant holds for every glyph.
    #[test]
    fn band_runs_never_straddle_a_texture_row() {
        let w = BAND_TEXTURE_WIDTH as usize;
        let mut cache = GlyphCache::new();

        // Rotating sizes so blocks land at many offsets relative to the boundary.
        let shapes = [(2u32, 2u32, 1usize), (4, 4, 3), (3, 5, 7), (6, 2, 13)];
        for i in 0..3000u32 {
            let (h, v, per) = shapes[i as usize % shapes.len()];
            cache.insert((0, i), synthetic(h, v, per));
        }

        assert!(
            cache.band_data().len() > w,
            "test must cross at least one row boundary to be meaningful",
        );

        for info in cache.glyphs.values() {
            let header_count =
                (info.band_max.0 + 1 + info.band_max.1 + 1) as usize;
            let origin = info.band_start.1 as usize * w + info.band_start.0 as usize;

            assert!(
                origin % w + header_count <= w,
                "header block straddles a row",
            );

            for b in 0..header_count {
                let hdr = cache.band_data()[origin + b];
                let count = hdr[0] as usize;
                let list_start = origin + hdr[1] as usize;
                assert!(
                    list_start % w + count <= w,
                    "band curve-loc list straddles a row",
                );
            }
        }
    }
}
