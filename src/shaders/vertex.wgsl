// Vertex shader for the Slug-style font renderer.
// Translated from Lengyel's reference HLSL/WGSL.

struct Params {
    matrix: mat4x4f,
}

@group(0) @binding(0) var<uniform> params: Params;

struct VsIn {
    @location(0) pos: vec2f,
    @location(1) glyph: vec2u,
    @location(2) loc_em: vec2f,
    @location(3) bnd: vec4f,
    @location(4) col: vec4f,
}

struct VsOut {
    @builtin(position) position: vec4f,
    @location(0) color: vec4f,
    @location(1) texcoord: vec2f,
    @location(2) @interpolate(flat) banding: vec4f,
    @location(3) @interpolate(flat) glyph: vec4i,
}

@vertex
fn main(input: VsIn) -> VsOut {
    var out: VsOut;

    // Slug coverage expects glyph-local em coords; curve atlas is stored in em-space.
    out.texcoord = input.loc_em;
    out.position = params.matrix * vec4f(input.pos.x, input.pos.y, 0.0, 1.0);

    out.glyph = vec4i(
        i32(input.glyph.x & 0xFFFFu),
        i32(input.glyph.x >> 16u),
        i32(input.glyph.y & 0xFFFFu),
        i32(input.glyph.y >> 16u),
    );
    out.banding = input.bnd;
    out.color = input.col;
    return out;
}
