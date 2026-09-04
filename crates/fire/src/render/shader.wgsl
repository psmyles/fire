// Fire viewport shader (WGSL): a fullscreen-triangle vertex stage plus a fragment stage that is a
// direct port of the per-pixel pipeline — inverse-map the surface pixel into image space, sample
// (point when magnifying for crisp texels, anisotropic+mips when minifying), then exposure ->
// tonemap -> channel isolation -> checker composite, all in linear light. The `*Srgb` view of the
// swapchain handles the final sRGB encode.
//
// The HLSL original is kept under `reference/shader.hlsl`; the math here is identical, stage for
// stage. wgpu validates and compiles this at pipeline creation on every OS — there is no
// build-time shader step.
//
// The `Params` layout must stay in lockstep with the `Params` struct in gpu.rs (WGSL uniform
// rules: vec2 aligns to 8, vec4 to 16; the fields are ordered so nothing needs padding and the
// block is exactly 128 bytes, which gpu.rs asserts).
//
// One WGSL-specific rule shaped the sampling code: `textureSample` and the derivative builtins
// may only be used in *uniform* control flow, and every branch after the letterbox test depends
// on the pixel. The mapping is a pure uniform scale, so the LOD is known in closed form —
// `log2(inv_zoom)` — and the samples use explicit-gradient / explicit-level forms instead, which
// are allowed anywhere and give the same result the implicit forms would.

struct Params {
    img_size: vec2<f32>,    // frame rect in flipbook mode (fb_on), else whole image
    surf_size: vec2<f32>,
    pan: vec2<f32>,
    inv_zoom: f32,
    exposure: f32,
    channel: i32,           // 0=RGBA 1=R 2=G 3=B 4=A 5=RGB
    tonemap: i32,           // 0=Reinhard 1=ACES
    is_hdr: i32,
    has_image: i32,
    linear_sample: i32,     // 1=sample already linear, 0=sRGB-decode rgb in shader
    background: i32,        // 0=black 1=white 2=grey 3=checker (letterbox + transparency)
    outline: i32,           // 1=draw a 1px image-boundary outline
    fb_on: i32,             // 1=flipbook: img_size is a cell rect, sample cell_a/cell_b of the sheet
    clear_lin: vec4<f32>,
    sheet_size: vec2<f32>,  // whole texture (texels); flipbook cell offsets are in this space
    cell_a: vec2<f32>,      // frame-A cell origin (texels)
    cell_b: vec2<f32>,      // frame-B cell origin (== cell_a when not blending)
    fb_blend: f32,          // 0..1 crossfade toward frame B (0 = hard cut)
    fb_max_lod: f32,        // mip clamp so minified samples can't bleed across cells
    surf_origin: vec2<f32>, // image sub-rect's top-left in RENDER-TARGET px (see fs_main)
    oct_crop: f32,          // octagon overlay crop factor (0 = quad, 0.5 = diamond)
    oct_hide: f32,          // 0..1 fade of the image outside the octagon (0 = overlay off)
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp_aniso: sampler;
@group(0) @binding(3) var samp_point: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u)); // (0,0) (2,0) (0,2)
    return vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow(max((c + 0.055) / 1.055, vec3<f32>(0.0)), vec3<f32>(2.4));
    return mix(hi, lo, step(c, vec3<f32>(0.04045)));
}

fn reinhard(c: vec3<f32>) -> vec3<f32> {
    return c / (1.0 + c);
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return saturate((x * (a * x + b)) / (x * (c * x + d) + e));
}

// The viewport backdrop in linear light: solid black/white/40%-grey, or a Photoshop-style
// checkerboard keyed to the surface pixel. Used both for the letterbox around the image and as
// the composite behind transparent pixels, so a partly-transparent image reads consistently.
fn backdrop(sp: vec2<f32>) -> vec3<f32> {
    if (p.background == 0) {
        return vec3<f32>(0.0);
    }
    if (p.background == 1) {
        return vec3<f32>(1.0);
    }
    if (p.background == 2) {
        return srgb_to_linear(vec3<f32>(0.4));
    }
    let cell = floor(sp / 12.0);
    let s = cell.x + cell.y;
    let odd = (s - 2.0 * floor(s / 2.0)) >= 0.5;
    let v = select(0.45, 0.21, odd); // light/dark checker (linear)
    return vec3<f32>(v);
}

// Texture coords for a *point* tap on the texel containing `t`, aimed at that texel's centre.
//
// Sampling at `t / size` directly is a half-texel gamble: `t` lands on an exact texel boundary for
// every pixel at once whenever the mapping is integral, and the sampler's own float->fixed-point
// rounding then breaks the tie inconsistently from tap to tap — scattered rows/columns pick the
// neighbouring texel. Flooring to the texel and re-centring puts every tap a half texel clear of
// an edge, so no rounding anywhere in the sampler can change which texel is read.
fn texel_center(t: vec2<f32>, size: vec2<f32>) -> vec2<f32> {
    return (floor(t) + 0.5) / size;
}

// Sample the flipbook frame texel `f` (frame-local, 0..img_size) from the sheet cell at origin
// `cell`. Explicit-LOD: the sheet's mip chain averages across cell boundaries, so implicit mips
// would ghost neighbouring frames into a minified frame — clamp to `fb_max_lod`. A half-texel
// inset keeps bilinear/aniso taps inside the cell; magnify (inv_zoom<=1) stays crisp at mip 0.
fn sample_cell(f: vec2<f32>, cell: vec2<f32>) -> vec4<f32> {
    let t = cell + clamp(f, vec2<f32>(0.5), p.img_size - 0.5);
    let uv = t / p.sheet_size;
    var s: vec4<f32>;
    if (p.inv_zoom <= 1.0) {
        s = textureSampleLevel(tex, samp_point, texel_center(t, p.sheet_size), 0.0);
    } else {
        // The screen->texel mapping is a uniform scale by inv_zoom, so the LOD the hardware would
        // compute is exactly log2(inv_zoom).
        let lod = min(log2(p.inv_zoom), p.fb_max_lod);
        s = textureSampleLevel(tex, samp_aniso, uv, lod);
    }
    if (p.linear_sample == 0) {
        s = vec4<f32>(srgb_to_linear(s.rgb), s.a);
    }
    return s;
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    if (p.has_image == 0) {
        return p.clear_lin;
    }
    // `position` is in RENDER-TARGET space, not viewport space: a viewport parked below the
    // toolbar still hands us absolute client coordinates. Subtracting the sub-rect's origin puts
    // us back in the viewport's own frame, which is what every line below (centering, the
    // outline, the checkerboard) assumes.
    let sp = pos.xy - p.surf_origin;          // viewport pixel center (origin top-left)
    let ctr = p.surf_size * 0.5 + p.pan;
    let f = p.img_size * 0.5 + (sp - ctr) * p.inv_zoom;   // image texel coords

    // A 1px (screen-space) outline hugging the OUTSIDE of the image boundary, drawn in the
    // letterbox gutter so it never covers image content. `sd` is the box signed distance in
    // texels (>0 outside the image, <0 inside), `sd_px` the same in screen pixels: the outline
    // is the ring of pixels whose centers land within one screen pixel outside the boundary.
    // White on a black backdrop, else black, so it always contrasts.
    //
    // The window is biased inward by EPS to kill a degenerate case: with a naive (0, 1) window,
    // when an edge lands exactly on a column/row of pixel centers, neither neighbour passes and
    // that whole edge vanishes until a pan/zoom nudge breaks the tie. Any unit-length half-open
    // window catches exactly one center per row/column, so nothing else changes.
    let eps = 1.0 / 256.0;
    let sd = max(max(-f.x, f.x - p.img_size.x), max(-f.y, f.y - p.img_size.y));
    let sd_px = sd / p.inv_zoom;
    if (p.outline != 0 && sd_px > -eps && sd_px < 1.0 - eps) {
        let v = select(0.0, 1.0, p.background == 0);
        return vec4<f32>(v, v, v, 1.0);
    }
    if (f.x < 0.0 || f.y < 0.0 || f.x >= p.img_size.x || f.y >= p.img_size.y) {
        return vec4<f32>(backdrop(sp), 1.0);             // letterbox = chosen backdrop (frame rect)
    }

    var rgb: vec3<f32>;
    var a: f32;
    if (p.fb_on != 0) {
        // Flipbook: sample frame A of the sheet, crossfading toward frame B (sample_cell decodes).
        var s = sample_cell(f, p.cell_a);
        if (p.fb_blend > 0.0) {
            s = mix(s, sample_cell(f, p.cell_b), p.fb_blend);
        }
        rgb = s.rgb;
        a = s.a;
    } else {
        // f is inside [0, img_size) here (the letterbox branch above returned), so the point tap's
        // floor lands on a real texel and the CLAMP address mode never comes into it.
        var s: vec4<f32>;
        if (p.inv_zoom <= 1.0) {
            // magnify/1:1 -> crisp texels
            s = textureSampleLevel(tex, samp_point, texel_center(f, p.img_size), 0.0);
        } else {
            // minify -> mips + anisotropic. The footprint is exactly inv_zoom texels per pixel on
            // both axes; handing the sampler that gradient is what the implicit form would derive.
            let g = vec2<f32>(p.inv_zoom) / p.img_size;
            s = textureSampleGrad(tex, samp_aniso, f / p.img_size, vec2<f32>(g.x, 0.0), vec2<f32>(0.0, g.y));
        }
        rgb = s.rgb;
        a = s.a;
        if (p.linear_sample == 0) {
            rgb = srgb_to_linear(rgb);
        }
    }

    if (p.is_hdr != 0) {
        rgb = rgb * p.exposure;
        if (p.tonemap == 1) {
            rgb = aces(rgb);
        } else {
            rgb = reinhard(rgb);
        }
    }

    var outc: vec3<f32>;
    if (p.channel == 1) {
        outc = rgb.rrr;
    } else if (p.channel == 2) {
        outc = rgb.ggg;
    } else if (p.channel == 3) {
        outc = rgb.bbb;
    } else if (p.channel == 4) {
        outc = srgb_to_linear(vec3<f32>(a)).xxx;
    } else if (p.channel == 5) {
        // 5 = RGB: the color values on their own, as if the image were opaque. No composite, so a
        // transparent region shows whatever color it actually carries instead of the backdrop.
        outc = rgb;
    } else {
        // 0 = RGBA (alpha composited)
        outc = rgb;
        if (a < 0.999) {
            outc = backdrop(sp) * (1.0 - a) + rgb * a;
        }
    }

    // Octagon overlay "hide outside": fade pixels outside the octagon inscribed in the frame rect
    // toward the backdrop. Unity's octagon (see crate::octagon): midpoint vertices pinned at
    // (±0.5, 0)/(0, ±0.5), corner vertices at (±a, ±a) with a = 0.5·(1−crop). Each side's
    // half-plane reduces to q.x + k·q.y ≤ 0.5 (and its mirror) with q the |offset| from the frame
    // center and k = crop/(1−crop): k=0 is the quad, k=1 the diamond. In flipbook mode
    // `f`/`img_size` are already the frame rect, so the shape tracks the shown cell.
    if (p.oct_hide > 0.0) {
        let q = abs(f / p.img_size - 0.5);
        let k = p.oct_crop / max(1.0 - p.oct_crop, 0.5);
        if (max(q.x + k * q.y, q.y + k * q.x) > 0.5) {
            outc = mix(outc, backdrop(sp), p.oct_hide);
        }
    }
    return vec4<f32>(outc, 1.0);
}
