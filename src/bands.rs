//! Band division and curve sorting.
//!
//! Lengyel-style Slug layout:
//!
//! - **Curve atlas** is RGBA16F, two control points per texel. Curves on the
//!   same contour share endpoint texels: curve `i`'s `p1` is at texel `k`,
//!   `p2` at texel `k`'s zw, and `p3` at texel `k+1`'s xy — which is also
//!   curve `i+1`'s `p1`. A contour with N curves uses N+1 texels.
//!
//! - **Band atlas** is RG16U. For each glyph we store H then V band headers
//!   (count, offset), followed by packed curve-loc texels (col, row) for each
//!   band. Curves in each H band are sorted by `max_x` descending; each V
//!   band by `max_y` descending — pairing with the shader's early-out test.
//!
//! - Curves are bucketed into bands they touch, widened by a small em-space
//!   epsilon so coverage doesn't drop at fp band boundaries. Horizontal-line
//!   curves are excluded from H bands (and vertical from V) because they
//!   contribute nothing to the polynomial sweep in that direction.
//!
//! - The shader handles row-wrap when reading curve texels, so we never need
//!   to pad rows on the CPU side.

use std::cmp::Ordering;

use half::f16;

use crate::outline::GlyphOutlines;

pub const BAND_TEXTURE_WIDTH: u32 = 4096;
pub const CURVE_TEXTURE_WIDTH: u32 = 4096;
const MAX_BANDS: usize = 16;
/// Em-space slack added to curve extents when assigning to bands.
const BAND_EPS: f32 = 1.0 / 1024.0;

pub struct BandData {
    /// RGBA16F texels — two control points per texel, shared along contours.
    pub curve_texels: Vec<[u16; 4]>,
    /// RG16U texels — headers (count, offset) then curve-locs (col, row).
    pub band_texels: Vec<[u16; 2]>,
    /// (num_v_bands - 1, num_h_bands - 1).
    pub band_max: (u32, u32),
    pub bbox: (f32, f32, f32, f32),
}

/// Reusable scratch for `process_bands_with`. Reset on each call.
#[derive(Default)]
pub struct BandsScratch {
    flat: Vec<CurveRef>,
    counts: Vec<u32>,
    h_offsets: Vec<u32>,
    h_indices: Vec<u32>,
    v_offsets: Vec<u32>,
    v_indices: Vec<u32>,
}

#[derive(Clone, Copy)]
struct CurveRef {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
    /// True when curve degenerates to a horizontal line: skip from H bands.
    is_h: bool,
    /// True when curve degenerates to a vertical line: skip from V bands.
    is_v: bool,
    /// Local texel index of this curve's `p1` within the glyph's curve block.
    texel_index: u32,
}

pub fn process_bands_with(outlines: &GlyphOutlines, scratch: &mut BandsScratch) -> BandData {
    let (min_x, min_y, max_x, max_y) = outlines.bounding_box();
    let bbox = (min_x, min_y, max_x, max_y);
    let bw = max_x - min_x;
    let bh = max_y - min_y;

    let mut curve_texels: Vec<[u16; 4]> = Vec::new();
    scratch.flat.clear();
    for contour in &outlines.contours {
        if contour.is_empty() {
            continue;
        }
        let contour_base = curve_texels.len() as u32;
        for (i, c) in contour.iter().enumerate() {
            scratch.flat.push(CurveRef {
                min_x: c.min_x(),
                max_x: c.max_x(),
                min_y: c.min_y(),
                max_y: c.max_y(),
                is_h: c.is_horizontal(),
                is_v: c.is_vertical(),
                texel_index: contour_base + i as u32,
            });
            curve_texels.push(pack(c.p1, c.p2));
        }
        let last = contour.last().unwrap();
        curve_texels.push(pack(last.p3, (0.0, 0.0)));
    }

    let num_curves = scratch.flat.len();
    let (num_h_bands, num_v_bands) = pick_band_counts(num_curves, bw, bh);
    let inv_w_x = if bw > 0.0 {
        num_v_bands as f32 / bw
    } else {
        0.0
    };
    let inv_w_y = if bh > 0.0 {
        num_h_bands as f32 / bh
    } else {
        0.0
    };

    let flat = &scratch.flat;
    partition(
        num_curves,
        num_h_bands,
        |i| {
            let m = flat[i];
            if m.is_h {
                None
            } else {
                Some(band_range(
                    m.min_y - min_y,
                    m.max_y - min_y,
                    inv_w_y,
                    num_h_bands,
                ))
            }
        },
        |a, b| {
            flat[b]
                .max_x
                .partial_cmp(&flat[a].max_x)
                .unwrap_or(Ordering::Equal)
        },
        &mut scratch.counts,
        &mut scratch.h_offsets,
        &mut scratch.h_indices,
    );
    partition(
        num_curves,
        num_v_bands,
        |i| {
            let m = flat[i];
            if m.is_v {
                None
            } else {
                Some(band_range(
                    m.min_x - min_x,
                    m.max_x - min_x,
                    inv_w_x,
                    num_v_bands,
                ))
            }
        },
        |a, b| {
            flat[b]
                .max_y
                .partial_cmp(&flat[a].max_y)
                .unwrap_or(Ordering::Equal)
        },
        &mut scratch.counts,
        &mut scratch.v_offsets,
        &mut scratch.v_indices,
    );

    let header_texels = (num_h_bands + num_v_bands) as u32;
    let total_locs = scratch.h_indices.len() + scratch.v_indices.len();
    let mut band_texels: Vec<[u16; 2]> = Vec::with_capacity(header_texels as usize + total_locs);

    let mut running = header_texels;
    for b in 0..num_h_bands {
        let count = scratch.h_offsets[b + 1] - scratch.h_offsets[b];
        band_texels.push([count as u16, running as u16]);
        running += count;
    }
    for b in 0..num_v_bands {
        let count = scratch.v_offsets[b + 1] - scratch.v_offsets[b];
        band_texels.push([count as u16, running as u16]);
        running += count;
    }
    for &ci in &scratch.h_indices {
        let t = scratch.flat[ci as usize].texel_index;
        band_texels.push(curve_texel_coord(t));
    }
    for &ci in &scratch.v_indices {
        let t = scratch.flat[ci as usize].texel_index;
        band_texels.push(curve_texel_coord(t));
    }

    BandData {
        curve_texels,
        band_texels,
        band_max: (
            (num_v_bands as u32).saturating_sub(1),
            (num_h_bands as u32).saturating_sub(1),
        ),
        bbox,
    }
}

fn pack(a: (f32, f32), b: (f32, f32)) -> [u16; 4] {
    [
        f16::from_f32(a.0).to_bits(),
        f16::from_f32(a.1).to_bits(),
        f16::from_f32(b.0).to_bits(),
        f16::from_f32(b.1).to_bits(),
    ]
}

fn partition<R, S>(
    n: usize,
    num_bands: usize,
    mut range: R,
    mut sort_cmp: S,
    counts: &mut Vec<u32>,
    offsets: &mut Vec<u32>,
    indices: &mut Vec<u32>,
) where
    R: FnMut(usize) -> Option<(usize, usize)>,
    S: FnMut(usize, usize) -> Ordering,
{
    counts.clear();
    counts.resize(num_bands, 0);
    for i in 0..n {
        if let Some((lo, hi)) = range(i) {
            for count in &mut counts[lo..=hi] {
                *count += 1;
            }
        }
    }
    offsets.clear();
    offsets.resize(num_bands + 1, 0);
    let mut acc = 0u32;
    for b in 0..num_bands {
        offsets[b] = acc;
        acc += counts[b];
    }
    offsets[num_bands] = acc;
    for c in counts.iter_mut() {
        *c = 0;
    }
    indices.clear();
    indices.resize(acc as usize, 0);
    for i in 0..n {
        if let Some((lo, hi)) = range(i) {
            for b in lo..=hi {
                let pos = (offsets[b] + counts[b]) as usize;
                indices[pos] = i as u32;
                counts[b] += 1;
            }
        }
    }
    for b in 0..num_bands {
        let s = offsets[b] as usize;
        let e = offsets[b + 1] as usize;
        indices[s..e].sort_by(|&a, &c| sort_cmp(a as usize, c as usize));
    }
}

fn band_range(rel_min: f32, rel_max: f32, inv_w: f32, num_bands: usize) -> (usize, usize) {
    if num_bands <= 1 {
        return (0, 0);
    }
    let last = num_bands as i32 - 1;
    let eps_scaled = BAND_EPS * inv_w;
    let lo = ((rel_min * inv_w) - eps_scaled).floor() as i32;
    let hi = ((rel_max * inv_w) + eps_scaled).floor() as i32;
    let lo = lo.clamp(0, last) as usize;
    let hi = hi.clamp(0, last) as usize;
    (lo, hi)
}

fn pick_band_counts(num_curves: usize, bw: f32, bh: f32) -> (usize, usize) {
    let base = (num_curves / 4).clamp(1, MAX_BANDS) as f32;
    if bw < 1e-6 || bh < 1e-6 {
        let n = base as usize;
        return (n, n);
    }
    let r = (bw / bh).sqrt();
    let max_b = MAX_BANDS as i32;
    let n_v = ((base * r).round() as i32).clamp(1, max_b) as usize;
    let n_h = ((base / r).round() as i32).clamp(1, max_b) as usize;
    (n_h, n_v)
}

fn curve_texel_coord(index: u32) -> [u16; 2] {
    [
        (index % CURVE_TEXTURE_WIDTH) as u16,
        (index / CURVE_TEXTURE_WIDTH) as u16,
    ]
}
