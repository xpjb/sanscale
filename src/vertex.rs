//! Vertex format and quad generation for text rendering.
//!
//! `pos` is in window pixels (`0..width`, `0..height`). `loc_em` stays in glyph-local
//! em coordinates for the Slug fragment shader (curve atlas is em-space). Y matches the
//! old `scale(_, -font_size, _)` convention via `pen_y - ey * size`.

use bytemuck::{Pod, Zeroable};

use crate::cache::GlyphInfo;

/// 56 bytes per vertex. Layout matches the WGSL shader.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct TextVertex {
    /// Window-space pixel position (matches orthographic `0..width`, `0..height`).
    pub pos: [f32; 2],
    /// Packed glyph metadata: band start and band max.
    pub glyph: [u32; 2],
    /// Glyph-local em-space offset from the pen (Slug sampling coord).
    pub loc_em: [f32; 2],
    /// bnd = (band_scale_x, band_scale_y, band_offset_x, band_offset_y) in em-space.
    pub bnd: [f32; 4],
    /// col = RGBA.
    pub col: [f32; 4],
}

pub(crate) fn push_glyph_quad_pixels(
    out: &mut Vec<TextVertex>,
    info: &GlyphInfo,
    pen_x: f32,
    pen_y: f32,
    size: f32,
    color: [f32; 4],
) {
    out.extend_from_slice(&glyph_quad_pixels(info, pen_x, pen_y, size, color));
}

fn glyph_quad_pixels(
    info: &GlyphInfo,
    pen_x: f32,
    pen_y: f32,
    size: f32,
    color: [f32; 4],
) -> [TextVertex; 6] {
    let (min_x, min_y, max_x, max_y) = info.bbox;
    let bw = max_x - min_x;
    let bh = max_y - min_y;
    let scale_x = if bw > 0.0001 {
        (info.band_max.0 + 1) as f32 / bw
    } else {
        1.0
    };
    let scale_y = if bh > 0.0001 {
        (info.band_max.1 + 1) as f32 / bh
    } else {
        1.0
    };
    let bnd = [scale_x, scale_y, -min_x * scale_x, -min_y * scale_y];
    let glyph = [
        (info.band_start.1 << 16) | info.band_start.0,
        ((info.band_max.1 & 0xFF) << 16) | (info.band_max.0 & 0xFF),
    ];
    let v = |ex, ey| TextVertex {
        pos: [pen_x + ex * size, pen_y - ey * size],
        glyph,
        loc_em: [ex, ey],
        bnd,
        col: color,
    };
    [
        v(min_x, min_y),
        v(max_x, min_y),
        v(max_x, max_y),
        v(min_x, min_y),
        v(max_x, max_y),
        v(min_x, max_y),
    ]
}

/// Textured-quad vertex for the emoji atlas. `pos` is window pixels; `uv` is in
/// atlas **texels** (the fragment shader normalizes by the texture size).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct EmojiVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
}

/// Emit two triangles for a color glyph: a `size`×`size` square whose baseline
/// (em y=0) sits at `pen_y` and whose left edge is `pen_x`, sampling the atlas
/// `uv_min`(top-left)..`uv_max`(bottom-right) rect.
pub(crate) fn push_emoji_quad(
    out: &mut Vec<EmojiVertex>,
    pen_x: f32,
    pen_y: f32,
    size: f32,
    uv_min: [f32; 2],
    uv_max: [f32; 2],
) {
    let (x0, x1) = (pen_x, pen_x + size);
    let (y0, y1) = (pen_y - size, pen_y); // top (em y=1) .. baseline (em y=0)
    let v = |x, y, u, w| EmojiVertex {
        pos: [x, y],
        uv: [u, w],
    };
    out.extend_from_slice(&[
        v(x0, y0, uv_min[0], uv_min[1]),
        v(x1, y0, uv_max[0], uv_min[1]),
        v(x1, y1, uv_max[0], uv_max[1]),
        v(x0, y0, uv_min[0], uv_min[1]),
        v(x1, y1, uv_max[0], uv_max[1]),
        v(x0, y1, uv_min[0], uv_max[1]),
    ]);
}
