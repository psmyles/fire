// Phase-3 image shader: view-transformed quad + the display pipeline.
//
// Vertex: maps a unit quad to the image's on-screen rectangle. pan/zoom/fit are baked
// into `view.transform` on the CPU (see render::uniforms) so the math stays unit-tested
// in Rust and the GPU just does `ndc = pos * scale + offset`.
//
// Fragment, in order (§ Phase 3): sample -> linearize (16-bit path) -> exposure 2^stops
// (HDR) -> tonemap Reinhard/ACES (HDR) -> channel isolation -> checkerboard composite ->
// optional sRGB encode. The pipeline runs in LINEAR light; output is linear and the sRGB
// surface encodes on present (FLAG_SRGB_ENCODE is set only when the surface is not sRGB).

struct View {
    transform: vec4<f32>,      // sx, sy, tx, ty
    image_size: vec4<f32>,     // w, h, 1/w, 1/h
    viewport_size: vec4<f32>,  // w, h, _, _
    bg_color: vec4<f32>,       // linear rgb, a
    params: vec4<f32>,         // exposure, checker_size, _, _
    modes: vec4<u32>,          // tonemap, channel, flags, _
};

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var image_tex: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

const FLAG_HDR: u32 = 1u;
const FLAG_SRGB_DECODE: u32 = 2u;
const FLAG_SRGB_ENCODE: u32 = 4u;
const FLAG_CHECKER: u32 = 8u;

const CH_RGB: u32 = 0u;
const CH_R: u32 = 1u;
const CH_G: u32 = 2u;
const CH_B: u32 = 3u;
const CH_A: u32 = 4u;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Unit quad as a triangle strip: TL, TR, BL, BR. uv 0,0 = image top-left, which lines
    // up with the texture's top-left, so no Y flip is needed.
    var quad = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
    );
    let p = quad[vid];
    var out: VsOut;
    out.pos = vec4<f32>(
        p.x * view.transform.x + view.transform.z,
        p.y * view.transform.y + view.transform.w,
        0.0,
        1.0,
    );
    out.uv = p;
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let x = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = x * 12.92;
    let hi = 1.055 * pow(x, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, x <= vec3<f32>(0.0031308));
}

fn reinhard(c: vec3<f32>) -> vec3<f32> {
    return c / (vec3<f32>(1.0) + c);
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    // Narkowicz 2015 ACES filmic fit.
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn checker(frag: vec2<f32>, size: f32) -> vec3<f32> {
    let cell = floor(frag / max(size, 1.0));
    let odd = (cell.x + cell.y) - 2.0 * floor((cell.x + cell.y) * 0.5);
    // Two neutral grays, given in linear so they read as a light/medium checker after the
    // surface's sRGB encode.
    let light = vec3<f32>(0.45);
    let dark = vec3<f32>(0.21);
    return select(dark, light, odd < 0.5);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let flags = view.modes.z;
    var s = textureSample(image_tex, image_sampler, in.uv);

    // 16-bit unorm stores sRGB-encoded values in a linear texture: linearize here. (The
    // 8-bit path samples an _Srgb texture and is linearized by the hardware.)
    if ((flags & FLAG_SRGB_DECODE) != 0u) {
        s = vec4<f32>(srgb_to_linear(s.rgb), s.a);
    }

    var rgb = s.rgb;
    let alpha = s.a;

    // HDR: exposure, then tonemap to SDR (float sources only).
    if ((flags & FLAG_HDR) != 0u) {
        rgb = rgb * exp2(view.params.x);
        if (view.modes.x == 1u) {
            rgb = aces(rgb);
        } else {
            rgb = reinhard(rgb);
        }
    }

    // Channel isolation. A solo color channel shows as grayscale of its (display-ready)
    // value; soloing alpha shows coverage as gray. For color channels the surface's sRGB
    // encode inverts the sample-time decode, so the stored byte reads back as that gray;
    // alpha is never sRGB-coded, so we linearize it to read back the same way.
    let ch = view.modes.y;
    var out_rgb: vec3<f32>;
    var composite = false;
    if (ch == CH_RGB) {
        out_rgb = rgb;
        composite = true;
    } else if (ch == CH_R) {
        out_rgb = vec3<f32>(rgb.r);
    } else if (ch == CH_G) {
        out_rgb = vec3<f32>(rgb.g);
    } else if (ch == CH_B) {
        out_rgb = vec3<f32>(rgb.b);
    } else {
        out_rgb = srgb_to_linear(vec3<f32>(alpha));
    }

    // Composite over the checkerboard (or solid bg) only in RGB mode with transparency.
    var final_rgb = out_rgb;
    if (composite) {
        var bg = view.bg_color.rgb;
        if ((flags & FLAG_CHECKER) != 0u) {
            bg = checker(in.pos.xy, view.params.y);
        }
        final_rgb = mix(bg, out_rgb, clamp(alpha, 0.0, 1.0));
    }

    if ((flags & FLAG_SRGB_ENCODE) != 0u) {
        final_rgb = linear_to_srgb(final_rgb);
    }
    return vec4<f32>(final_rgb, 1.0);
}
