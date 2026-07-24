// Emoji atlas quads: transform window-pixel position by the ortho matrix, pass
// through atlas texel coordinates (normalized in the fragment shader).

struct Params {
    matrix: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> params: Params;

struct VsOut {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn main(@location(0) pos: vec2f, @location(1) uv: vec2f) -> VsOut {
    var out: VsOut;
    out.position = params.matrix * vec4f(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}
