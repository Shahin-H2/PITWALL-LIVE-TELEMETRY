// Dual Kawase blur.
//
// Chosen over a separable Gaussian on purpose. A Gaussian wide enough to look
// like real frosted glass needs a large kernel and two full-resolution passes;
// dual Kawase reaches a visually equivalent result by ping-ponging between
// progressively smaller mips with a fixed 5/8-tap kernel, so cost falls
// geometrically with each level. At 4 down + 4 up passes the effective radius
// is enormous and the total sampled area is a fraction of the screen.
//
// This is the technique desktop compositors ship for exactly this effect.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct Params {
    // Texel size of the *source* texture, plus a radius scale.
    texel: vec2<f32>,
    offset: f32,
    _pad: f32,
};
@group(0) @binding(2) var<uniform> p: Params;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vi & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

// Downsample: 1 centre tap at double weight + 4 diagonal taps.
@fragment
fn fs_down(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    // The destination is half the source size, so reconstruct source UV from
    // the destination fragment position.
    let uv = frag.xy * p.texel * 2.0;
    let o = p.texel * p.offset;

    var sum = textureSample(src, samp, uv) * 4.0;
    sum += textureSample(src, samp, uv + vec2<f32>(-o.x, -o.y));
    sum += textureSample(src, samp, uv + vec2<f32>( o.x, -o.y));
    sum += textureSample(src, samp, uv + vec2<f32>(-o.x,  o.y));
    sum += textureSample(src, samp, uv + vec2<f32>( o.x,  o.y));
    return sum / 8.0;
}

// Upsample: 8 taps in a rotated-box arrangement, weighted 1/12 and 2/12.
@fragment
fn fs_up(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    // Destination is double the source size.
    let uv = frag.xy * p.texel * 0.5;
    let o = p.texel * p.offset;

    var sum = textureSample(src, samp, uv + vec2<f32>(-o.x * 2.0, 0.0));
    sum += textureSample(src, samp, uv + vec2<f32>(-o.x,  o.y)) * 2.0;
    sum += textureSample(src, samp, uv + vec2<f32>( 0.0,  o.y * 2.0));
    sum += textureSample(src, samp, uv + vec2<f32>( o.x,  o.y)) * 2.0;
    sum += textureSample(src, samp, uv + vec2<f32>( o.x * 2.0, 0.0));
    sum += textureSample(src, samp, uv + vec2<f32>( o.x, -o.y)) * 2.0;
    sum += textureSample(src, samp, uv + vec2<f32>( 0.0, -o.y * 2.0));
    sum += textureSample(src, samp, uv + vec2<f32>(-o.x, -o.y)) * 2.0;
    return sum / 12.0;
}
