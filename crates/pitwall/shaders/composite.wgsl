// Instanced SDF UI renderer.
//
// Every element on screen — glass panels, gauge arcs, pedal bars, text — is
// one instance of the same quad, drawn in one pass, branching on a shape tag.
// Nothing is a mesh and nothing is a pre-rendered image, so the whole UI is
// resolution-independent and analytically antialiased: it is exactly as crisp
// on a 5K display as on a laptop panel, with no mip chain and no atlas seams.
//
// Shapes are signed distance fields. The distance field is not just a way to
// get a shape — it is *reused* for the lighting. `sd_round_box` gives us
// distance-to-edge for free, which drives the glass rim highlight, the inner
// shadow, and the refraction offset. That reuse is what makes the glass cost
// almost nothing beyond the fill.

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
@group(0) @binding(1) var backdrop_tex: texture_2d<f32>;
@group(0) @binding(2) var linear_samp: sampler;
@group(0) @binding(3) var font_tex: texture_2d<f32>;

const SHAPE_GLASS: f32  = 0.0;
const SHAPE_FILL: f32   = 1.0;
const SHAPE_ARC: f32    = 2.0;
const SHAPE_GLYPH: f32  = 3.0;
const SHAPE_CIRCLE: f32 = 4.0;
const SHAPE_RING: f32   = 5.0;

const TAU: f32 = 6.28318530718;

struct VsIn {
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4<f32>,   // x, y, w, h  (pixels, top-left origin)
    @location(1) color: vec4<f32>,  // rgba, premultiply-free
    @location(2) params: vec4<f32>, // radius, a, b, c   (shape dependent)
    @location(3) shape_data: vec4<f32>,   // shape, glyph, softness, spare
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,   // pixel offset from the rect's centre
    @location(1) half_size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) shape_data: vec4<f32>,
    @location(5) screen_uv: vec2<f32>,
    @location(6) quad_uv: vec2<f32>, // 0..1 across the quad
};

@vertex
fn vs_main(v: VsIn) -> VsOut {
    // Two triangles from six indices, expanded by a small margin so the SDF
    // has room to antialias and the glass has room for its outer glow.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[v.vi];
    let margin = 3.0;

    let size = v.rect.zw + margin * 2.0;
    let origin = v.rect.xy - margin;
    let px = origin + c * size;

    var vs_out: VsOut;
    // Pixel space -> clip space. Y is flipped because the layout code thinks
    // in screen coordinates (origin top-left) and clip space does not.
    vs_out.pos = vec4<f32>(
        (px.x / u.resolution.x) * 2.0 - 1.0,
        1.0 - (px.y / u.resolution.y) * 2.0,
        0.0, 1.0
    );
    vs_out.half_size = v.rect.zw * 0.5;
    vs_out.local = px - (v.rect.xy + vs_out.half_size);
    vs_out.color = v.color;
    vs_out.params = v.params;
    vs_out.shape_data = v.shape_data;
    vs_out.screen_uv = px / u.resolution;
    vs_out.quad_uv = c;
    return vs_out;
}

// Signed distance to a rounded box. Negative inside.
fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let rr = min(r, min(b.x, b.y));
    let q = abs(p) - b + rr;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - rr;
}

// Analytic gradient of the box SDF — the surface "normal" in 2D. Used to
// refract the backdrop, which is what separates glass from frosted plastic.
fn sd_round_box_grad(p: vec2<f32>, b: vec2<f32>, r: f32) -> vec2<f32> {
    let e = 1.0;
    let dx = sd_round_box(p + vec2<f32>(e, 0.0), b, r) - sd_round_box(p - vec2<f32>(e, 0.0), b, r);
    let dy = sd_round_box(p + vec2<f32>(0.0, e), b, r) - sd_round_box(p - vec2<f32>(0.0, e), b, r);
    return normalize(vec2<f32>(dx, dy) + vec2<f32>(1e-6));
}

// Antialiased coverage from a signed distance, using screen-space derivatives
// so the edge is exactly one pixel wide at any zoom or DPI.
fn aa(d: f32) -> f32 {
    let w = max(fwidth(d), 1e-5);
    return 1.0 - smoothstep(-w, w, d);
}

// Sample the glyph's signed distance field.
//
// The atlas stores distance, not coverage: 0.5 is exactly the glyph edge, and
// values rise toward 1.0 inside. Bilinear interpolation of a distance field
// reconstructs the edge correctly at any magnification, which is the entire
// reason this is not a bitmap.
fn glyph_sdf(idx: f32, quad_uv: vec2<f32>) -> f32 {
    let i = clamp(idx - 32.0, 0.0, 95.0);
    let col = i % 16.0;
    let row = floor(i / 16.0);
    let cell = vec2<f32>(1.0 / 16.0, 1.0 / 6.0);
    let uv = (vec2<f32>(col, row) + clamp(quad_uv, vec2<f32>(0.0), vec2<f32>(1.0))) * cell;
    return textureSample(font_tex, linear_samp, uv).r;
}

@fragment
fn fs_main(v: VsOut) -> @location(0) vec4<f32> {
    let shape = v.shape_data.x;
    let radius = v.params.x;

    // ---- text ------------------------------------------------------------
    if (shape == SHAPE_GLYPH) {
        // The quad is inset by the vertex margin, so re-derive the glyph's own
        // 0..1 cell space rather than using quad_uv directly. The atlas cell
        // already carries a unit of padding, so sampling slightly outside is
        // safe and simply reads as "far outside the glyph".
        let g = (v.local + v.half_size) / (v.half_size * 2.0);
        let d = glyph_sdf(v.shape_data.y, g);

        // `fwidth` is the screen-space rate of change of the distance, so this
        // window is exactly one pixel wide however far the glyph is magnified
        // or minified. That is what makes the edge razor-sharp at 400 px and
        // still properly antialiased at 10 px — one expression, no LOD bias,
        // no per-size assets.
        let w = max(fwidth(d), 1e-5);
        let fill = smoothstep(0.5 - w, 0.5 + w, d);

        // params.x widens a darker contour outside the fill, in distance
        // units. Free, because it is another threshold on the same sample —
        // and it is what keeps a light numeral legible over bright glass.
        let ow = v.params.x;
        let outer = smoothstep(0.5 - ow - w, 0.5 - ow + w, d);
        if (outer < 0.004) { discard; }

        let outline_rgb = vec3<f32>(0.0, 0.0, 0.0);
        let rgb = mix(outline_rgb, v.color.rgb, fill);
        return vec4<f32>(rgb, v.color.a * outer);
    }

    // ---- arcs and rings ---------------------------------------------------
    if (shape == SHAPE_ARC || shape == SHAPE_RING) {
        let r = min(v.half_size.x, v.half_size.y) - v.params.w * 0.5;
        let d = length(v.local);
        let ring = abs(d - r) - v.params.w * 0.5;

        var cov = aa(ring);
        if (shape == SHAPE_ARC) {
            // Angle in turns, 0 at 12 o'clock, increasing clockwise.
            var ang = atan2(v.local.x, -v.local.y) / TAU;
            if (ang < 0.0) { ang = ang + 1.0; }
            let a0 = v.params.y;
            let a1 = v.params.z;

            // Work in arc length *from the start*, wrapped into [0,1). A naive
            // `ang >= a0 && ang <= a1` cannot express a sweep that crosses
            // twelve o'clock — which every tachometer does, since it runs from
            // the lower left up over the top to the lower right. Rebasing on
            // a0 makes the wrapping case ordinary instead of special.
            var rel = ang - a0;
            if (rel < 0.0) { rel = rel + 1.0; }
            var span = a1 - a0;
            if (span < 0.0) { span = span + 1.0; }

            // Feather the sweep ends by about one pixel of arc length.
            let feather = max(1.5 / max(r * TAU, 1.0), 1e-4);
            let sweep = smoothstep(-feather, feather, rel)
                      * (1.0 - smoothstep(span - feather, span + feather, rel));
            cov = cov * sweep;
        }
        if (cov < 0.003) { discard; }
        return vec4<f32>(v.color.rgb, v.color.a * cov);
    }

    // ---- circles ----------------------------------------------------------
    if (shape == SHAPE_CIRCLE) {
        let r = min(v.half_size.x, v.half_size.y);
        let d = length(v.local) - r;
        let cov = aa(d);
        if (cov < 0.003) { discard; }
        return vec4<f32>(v.color.rgb, v.color.a * cov);
    }

    // ---- solid rounded rect ----------------------------------------------
    let d = sd_round_box(v.local, v.half_size, radius);

    if (shape == SHAPE_FILL) {
        let cov = aa(d);
        if (cov < 0.003) { discard; }
        return vec4<f32>(v.color.rgb, v.color.a * cov);
    }

    // ---- glass ------------------------------------------------------------
    // Four ingredients, in the order they matter:
    //   1. a wide-blurred backdrop, sampled through a refraction offset
    //   2. a Fresnel-ish rim that brightens where the surface turns away
    //   3. a soft inner shadow, so the panel reads as a solid slab
    //   4. an outer glow, which is what makes it float above the backdrop
    let cov = aa(d);
    if (cov < 0.003) { discard; }

    let grad = sd_round_box_grad(v.local, v.half_size, radius);

    // Refraction: bend the backdrop sample near the edges, as a real bevelled
    // slab would. Strength falls off with distance from the edge, so the
    // centre of the panel is undistorted.
    let edge_prox = exp(d * 0.14);                       // ~1 at the edge, ->0 inside
    let refract_px = grad * edge_prox * 14.0 * v.params.y;
    let uv = clamp(v.screen_uv + refract_px / u.resolution, vec2<f32>(0.001), vec2<f32>(0.999));
    var bg = textureSample(backdrop_tex, linear_samp, uv).rgb;

    // Tint the transmitted light toward the panel colour. `params.z` is the
    // opacity of the tint: 0 = clear glass, 1 = solid.
    bg = mix(bg, v.color.rgb, v.params.z);

    // Hover, 0..1, springs in from the CPU side.
    let hover = v.shape_data.z;

    // Fresnel rim. `edge` peaks in a narrow band just inside the boundary.
    // Hovering widens and brightens it: the panel appears to catch more light
    // as the cursor approaches, which is the whole illusion of a lit surface.
    let rim_w = 1.6 + v.params.w + hover * 5.0;
    let edge = 1.0 - smoothstep(0.0, rim_w, -d);
    let rim = pow(clamp(edge, 0.0, 1.0), 2.2) * (1.0 + hover * 1.35);

    // The rim is brighter on the upper-left, as though lit from above — the
    // cue that reads as "this has thickness".
    let light_dir = normalize(vec2<f32>(-0.55, -0.85));
    let facing = clamp(dot(grad, light_dir) * 0.5 + 0.5, 0.0, 1.0);
    let rim_col = vec3<f32>(0.80, 0.88, 1.0) * (0.30 + 0.70 * facing);

    // Inner shadow just inside the edge, opposite the light.
    let inner = smoothstep(0.0, 26.0, -d);
    let shade = mix(0.82, 1.0, inner);

    var col = bg * shade + rim_col * rim * 0.42;

    // A soft interior bloom on hover, brightest at the top edge, so the panel
    // reads as lit from within rather than merely outlined.
    col += vec3<f32>(0.32, 0.47, 0.66) * hover * 0.16 * (1.0 - v.quad_uv.y * 0.55);

    // A very slight specular sheen across the top third, animated by the
    // limiter pulse so the whole cluster flares on the rev limit.
    let sheen = exp(-pow((v.quad_uv.y - 0.18) * 4.0, 2.0)) * 0.05;
    col += vec3<f32>(1.0) * sheen * (0.5 + 0.5 * u.shift_pulse * u.limiter);

    return vec4<f32>(col, v.color.a * cov);
}
