// Backdrop pass: the surface the glass refracts.
//
// Rendered once into an offscreen target, then blurred. It deliberately
// contains large, slow-moving luminance gradients — glass looks like glass
// because of what is *behind* it, and a flat background blurs to a flat
// background, which reads as frosted plastic.

struct Uniforms {
    resolution: vec2<f32>,
    time: f32,
    dpi: f32,

    rpm_frac: f32,
    throttle: f32,
    brake: f32,
    steer: f32,

    speed_norm: f32,
    gear: f32,
    lat_g: f32,
    long_g: f32,

    balance: f32,
    connected: f32,
    sim_id: f32,
    frame_ms: f32,

    limiter: f32,
    shift_pulse: f32,
    slip: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle. Cheaper than a quad: no diagonal seam, one
    // primitive, and the GPU never rasterises a helper-invocation-heavy edge
    // down the middle of the screen.
    let x = f32((vi << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vi & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = frag.xy / u.resolution;
    let aspect = u.resolution.x / max(u.resolution.y, 1.0);
    let p = vec2<f32>((uv.x - 0.5) * aspect, uv.y - 0.5);

    // Deep base, slightly blue — a neutral dark that doesn't tint the glass.
    var col = vec3<f32>(0.043, 0.051, 0.067);

    // A vignette that lifts the centre, so panels near the middle have
    // something to catch.
    let vig = 1.0 - smoothstep(0.0, 0.9, length(p));
    col += vec3<f32>(0.03, 0.038, 0.055) * vig;

    // Two slow orbiting light pools. Their motion is what makes the blurred
    // backdrop feel alive rather than static — the glass "breathes".
    let t = u.time * 0.08;
    let a = vec2<f32>(cos(t) * 0.42, sin(t * 0.83) * 0.26);
    let b = vec2<f32>(cos(t * 1.31 + 2.1) * 0.38, sin(t * 0.67 + 1.4) * 0.3);

    let da = max(length(p - a), 0.001);
    let db = max(length(p - b), 0.001);
    col += vec3<f32>(0.10, 0.32, 0.52) * (0.055 / (da * 2.2 + 0.12));
    col += vec3<f32>(0.45, 0.13, 0.30) * (0.040 / (db * 2.4 + 0.14));

    // Speed-reactive warmth: the backdrop energises as the car does. Subtle
    // by design — it should register peripherally, never compete with the
    // instruments.
    col += vec3<f32>(0.16, 0.10, 0.03) * u.speed_norm * 0.30 * vig;

    // Limiter flash bleeds into the backdrop so the whole cluster reacts,
    // not just the tacho.
    col += vec3<f32>(0.5, 0.10, 0.05) * u.limiter * u.shift_pulse * 0.16;

    // A faint horizon band, for a sense of ground plane.
    let band = exp(-abs(uv.y - 0.72) * 9.0);
    col += vec3<f32>(0.05, 0.07, 0.11) * band * 0.5;

    // Dither before the blur. Without this, the wide smooth gradients above
    // band visibly on an OLED once they're blurred — banding is created by
    // quantisation, so it has to be broken up before the 8-bit store.
    col += (hash(frag.xy) - 0.5) * (1.0 / 255.0);

    return vec4<f32>(col, 1.0);
}
