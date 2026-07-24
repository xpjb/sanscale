// Sample the emoji atlas (premultiplied RGBA). `uv` arrives as atlas texels;
// normalize by the actual texture size so the power-of-two capacity height does
// not distort sampling. The pipeline blends with (ONE, ONE_MINUS_SRC_ALPHA).

@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct FsIn {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

@fragment
fn main(in: FsIn) -> @location(0) vec4f {
    let dims = vec2f(textureDimensions(atlas_tex));
    return textureSample(atlas_tex, atlas_sampler, in.uv / dims);
}
