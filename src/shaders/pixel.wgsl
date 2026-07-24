// Pixel shader: per-pixel analytic coverage by integrating curves in
// horizontal and vertical bands. Lengyel 2017, "Slug".

fn calc_root_code(y1: f32, y2: f32, y3: f32) -> u32 {
    let i1 = (bitcast<u32>(y1) >> 31u);
    let i2 = (bitcast<u32>(y2) >> 30u);
    let i3 = (bitcast<u32>(y3) >> 29u);
    var shift = (i2 & 2u) | (i1 & ~2u);
    shift = (i3 & 4u) | (shift & ~4u);
    return (0x2E74u >> shift) & 0x0101u;
}

fn solve_horiz_poly(p12: vec4f, p3: vec2f) -> vec2f {
    let a = p12.xy - p12.zw * 2.0 + p3;
    let b = p12.xy - p12.zw;
    let ra = 1.0 / a.y;
    let rb = 0.5 / b.y;
    let d = sqrt(max(b.y * b.y - a.y * p12.y, 0.0));
    var t1 = (b.y - d) * ra;
    var t2 = (b.y + d) * ra;
    if abs(a.y) < 1.0 / 65536.0 {
        t1 = p12.y * rb;
        t2 = p12.y * rb;
    }
    return vec2f(
        (a.x * t1 - b.x * 2.0) * t1 + p12.x,
        (a.x * t2 - b.x * 2.0) * t2 + p12.x,
    );
}

fn solve_vert_poly(p12: vec4f, p3: vec2f) -> vec2f {
    let a = p12.xy - p12.zw * 2.0 + p3;
    let b = p12.xy - p12.zw;
    let ra = 1.0 / a.x;
    let rb = 0.5 / b.x;
    let d = sqrt(max(b.x * b.x - a.x * p12.x, 0.0));
    var t1 = (b.x - d) * ra;
    var t2 = (b.x + d) * ra;
    if abs(a.x) < 1.0 / 65536.0 {
        t1 = p12.x * rb;
        t2 = p12.x * rb;
    }
    return vec2f(
        (a.y * t1 - b.y * 2.0) * t1 + p12.y,
        (a.y * t2 - b.y * 2.0) * t2 + p12.y,
    );
}

fn calc_band_loc(glyph_loc: vec2i, offset: u32) -> vec2i {
    let total = u32(glyph_loc.x) + offset;
    let col = i32(total % 4096u);
    let row_delta = i32(total / 4096u);
    return vec2i(col, glyph_loc.y + row_delta);
}

// Curve atlas is 4096 wide; sequential texels along a contour may wrap rows.
fn fetch_curve(curve_tex: texture_2d<f32>, loc: vec2i, off: i32) -> vec4f {
    let total = loc.x + off;
    let col = total & 4095;
    let row_delta = total >> 12;
    return textureLoad(curve_tex, vec2i(col, loc.y + row_delta), 0);
}

fn calc_coverage(xcov: f32, ycov: f32, xwgt: f32, ywgt: f32) -> f32 {
    var coverage = max(
        abs(xcov * xwgt + ycov * ywgt) / max(xwgt + ywgt, 1.0 / 65536.0),
        min(abs(xcov), abs(ycov)),
    );
    return clamp(coverage, 0.0, 1.0);
}

fn slug_render(
    curve_tex: texture_2d<f32>,
    band_tex: texture_2d<u32>,
    render_coord: vec2f,
    band_transform: vec4f,
    glyph_data: vec4i,
) -> f32 {
    let ems_per_pixel = fwidth(render_coord);
    let pixels_per_em = 1.0 / ems_per_pixel;

    var band_max = glyph_data.zw;
    band_max.y = band_max.y & 0x00FF;

    let band_index = clamp(
        vec2i(
            i32(render_coord.x * band_transform.x + band_transform.z),
            i32(render_coord.y * band_transform.y + band_transform.w),
        ),
        vec2i(0, 0),
        band_max,
    );
    let glyph_loc = glyph_data.xy;

    var xcov = 0.0;
    var xwgt = 0.0;
    let hband_data = textureLoad(band_tex, vec2i(glyph_loc.x + band_index.y, glyph_loc.y), 0).xy;
    var hband_loc = calc_band_loc(glyph_loc, hband_data.y);

    for (var i: i32 = 0; i < i32(hband_data.x); i = i + 1) {
        let curve_loc_v = textureLoad(band_tex, vec2i(hband_loc.x + i, hband_loc.y), 0);
        let curve_loc = vec2i(i32(curve_loc_v.x), i32(curve_loc_v.y));
        let t0 = fetch_curve(curve_tex, curve_loc, 0);
        let t1 = fetch_curve(curve_tex, curve_loc, 1).xy;
        let p1 = t0.xy - render_coord;
        let p2 = t0.zw - render_coord;
        let p3 = t1 - render_coord;
        let p12 = vec4f(p1.x, p1.y, p2.x, p2.y);
        if max(max(p12.x, p12.z), p3.x) * pixels_per_em.x < -0.5 { break; }
        let code = calc_root_code(p12.y, p12.w, p3.y);
        if code != 0u {
            let r = solve_horiz_poly(p12, p3) * pixels_per_em.x;
            if (code & 1u) != 0u {
                xcov = xcov + clamp(r.x + 0.5, 0.0, 1.0);
                xwgt = max(xwgt, clamp(1.0 - abs(r.x) * 2.0, 0.0, 1.0));
            }
            if code > 1u {
                xcov = xcov - clamp(r.y + 0.5, 0.0, 1.0);
                xwgt = max(xwgt, clamp(1.0 - abs(r.y) * 2.0, 0.0, 1.0));
            }
        }
    }

    var ycov = 0.0;
    var ywgt = 0.0;
    let vband_data = textureLoad(
        band_tex,
        vec2i(glyph_loc.x + band_max.y + 1 + band_index.x, glyph_loc.y),
        0,
    ).xy;
    var vband_loc = calc_band_loc(glyph_loc, vband_data.y);

    for (var i: i32 = 0; i < i32(vband_data.x); i = i + 1) {
        let curve_loc_v = textureLoad(band_tex, vec2i(vband_loc.x + i, vband_loc.y), 0);
        let curve_loc = vec2i(i32(curve_loc_v.x), i32(curve_loc_v.y));
        let t0 = fetch_curve(curve_tex, curve_loc, 0);
        let t1 = fetch_curve(curve_tex, curve_loc, 1).xy;
        let p1 = t0.xy - render_coord;
        let p2 = t0.zw - render_coord;
        let p3 = t1 - render_coord;
        let p12 = vec4f(p1.x, p1.y, p2.x, p2.y);
        if max(max(p12.y, p12.w), p3.y) * pixels_per_em.y < -0.5 { break; }
        let code = calc_root_code(p12.x, p12.z, p3.x);
        if code != 0u {
            let r = solve_vert_poly(p12, p3) * pixels_per_em.y;
            if (code & 1u) != 0u {
                ycov = ycov - clamp(r.x + 0.5, 0.0, 1.0);
                ywgt = max(ywgt, clamp(1.0 - abs(r.x) * 2.0, 0.0, 1.0));
            }
            if code > 1u {
                ycov = ycov + clamp(r.y + 0.5, 0.0, 1.0);
                ywgt = max(ywgt, clamp(1.0 - abs(r.y) * 2.0, 0.0, 1.0));
            }
        }
    }

    return calc_coverage(xcov, ycov, xwgt, ywgt);
}

@group(1) @binding(0) var curve_texture: texture_2d<f32>;
@group(1) @binding(1) var band_texture: texture_2d<u32>;

struct FsIn {
    @builtin(position) position: vec4f,
    @location(0) color: vec4f,
    @location(1) texcoord: vec2f,
    @location(2) @interpolate(flat) banding: vec4f,
    @location(3) @interpolate(flat) glyph: vec4i,
}

@fragment
fn main(input: FsIn) -> @location(0) vec4f {
    let cov = slug_render(curve_texture, band_texture, input.texcoord, input.banding, input.glyph);
    return input.color * cov;
}
