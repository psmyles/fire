// Fire viewport shader: a fullscreen-triangle vertex shader (from gl_VertexIndex) plus a fragment
// shader that is a direct port of the former CPU per-pixel pipeline: inverse-map the surface pixel
// into image space, sample (point when magnifying for crisp texels, anisotropic+mips when
// minifying), then exposure -> tonemap -> channel isolation -> checker composite, all in linear
// light, and sRGB-encode on the way out.
//
// The encode is the shader's, not the render target's: sokol_gfx draws into the swapchain through
// one plain UNORM target, and Dear ImGui's colors are already sRGB, so the image pass cannot
// borrow an `*_SRGB` view of the same pixels the way the D3D11 shell did. Encoding the final
// linear color here gives the same bytes for the same pixels: nothing blends in this pass.
//
// THIS IS THE ONE SOURCE. It is written in sokol-shdc's annotated GLSL (Vulkan syntax: separate
// texture and sampler objects) and *generated* into HLSL for D3D11 and MSL for Metal, plus the
// sokol_gfx shader reflection, by `scripts/gen-shaders.sh`. The generated files under
// `render/generated/` are checked in; build.rs compiles this platform's pair to bytecode (fxc ->
// DXBC on Windows, `xcrun metal` -> .metallib on macOS), so there is no runtime shader compile on
// either OS and a broken shader is a build error. Edit this file, re-run the script, commit both.
//
// The uniform block must stay in lockstep with the `Params` struct in gpu.rs; gpu.rs asserts the
// two are the same size against the generated struct, which is the half a human cannot forget.

@vs vs_main
void main() {
    // (0,0) (2,0) (0,2) -> one triangle that covers the whole target.
    vec2 uv = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    gl_Position = vec4(uv * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
}
@end

@fs ps_main
layout(binding=0) uniform params {
    vec2  img_size;       // frame rect in flipbook mode (fb_on), else whole image
    vec2  surf_size;
    vec2  pan;
    float inv_zoom;
    float exposure;
    int   channel;        // 0=RGBA 1=R 2=G 3=B 4=A 5=RGB
    int   tonemap;        // 0=Reinhard 1=ACES
    int   is_hdr;
    int   has_image;
    int   linear_sample;  // 1=sample already linear, 0=sRGB-decode rgb in shader
    int   background;     // 0=black 1=white 2=grey 3=checker (letterbox + transparency)
    int   outline;        // 1=draw a 1px image-boundary outline
    int   fb_on;          // 1=flipbook: img_size is a cell rect, sample cell_a/cell_b of the sheet
    vec4  clear_lin;
    vec2  sheet_size;     // whole texture (texels); flipbook cell offsets are in this space
    vec2  cell_a;         // frame-A cell origin (texels)
    vec2  cell_b;         // frame-B cell origin (== cell_a when not blending)
    float fb_blend;       // 0..1 crossfade toward frame B (0 = hard cut)
    float fb_max_lod;     // mip clamp so minified samples can't bleed across cells
    vec2  surf_origin;    // image sub-rect's top-left in RENDER-TARGET px (see shade)
    float oct_crop;       // octagon overlay crop factor (0 = quad, 0.5 = diamond)
    float oct_hide;       // 0..1 fade of the image outside the octagon (0 = overlay off)
};

// Separate texture and sampler, combined at each tap. `samp_aniso` is the filtering/mip sampler
// (binding 0, s0 on D3D11), `samp_point` the nearest one (binding 1, s1).
layout(binding=0) uniform texture2D tex;
layout(binding=0) uniform sampler samp_aniso;
layout(binding=1) uniform sampler samp_point;

out vec4 frag_color;

vec3 srgb_to_linear(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(hi, lo, step(c, vec3(0.04045)));
}
vec3 linear_to_srgb(vec3 c) {
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(max(c, 0.0), vec3(1.0 / 2.4)) - 0.055;
    return mix(hi, lo, step(c, vec3(0.0031308)));
}
vec3 reinhard(vec3 c) { return c / (1.0 + c); }

// The viewport backdrop in linear light: solid black/white/40%-grey, or a Photoshop-style
// checkerboard keyed to the surface pixel. Used both for the letterbox around the image and as
// the composite behind transparent pixels, so a partly-transparent image reads consistently.
vec3 backdrop(vec2 sp) {
    if (background == 0) return vec3(0.0);
    if (background == 1) return vec3(1.0);
    if (background == 2) return srgb_to_linear(vec3(0.4));
    vec2 cell = floor(sp / 12.0);
    // `mod` where the HLSL said `fmod`: sp is inside the scissored sub-rect, so cell >= 0 and the
    // two agree; mod is also the better-behaved one if that ever stops being true.
    float v = (mod(cell.x + cell.y, 2.0) < 0.5) ? 0.45 : 0.21; // light/dark checker (linear)
    return vec3(v);
}
vec3 aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

// Texture coords for a *point* tap on the texel containing `t`, aimed at that texel's centre.
//
// Sampling at `t / size` directly is a half-texel gamble: `t` lands on an exact texel boundary for
// every pixel at once whenever the mapping is integral — 1:1 with an integral pan and `img_size` /
// `surf_size` of the same parity (i.e. any window whose viewport is even-sized for an even image),
// and every whole-number zoom above that. On the boundary the sampler's own float→fixed-point
// rounding breaks the tie, and because the rounding of `t / size` differs from tap to tap it breaks
// it *inconsistently*: scattered rows/columns pick the neighbouring texel while their neighbours
// don't. The image is the right size and roughly right, but fine detail — text especially — comes
// out with columns dropped or doubled at random, which is the whole reason to point-sample in the
// first place. Flooring to the texel and re-centring puts every tap a half texel clear of an edge,
// so no rounding anywhere in the sampler can change which texel is read.
vec2 texel_center(vec2 t, vec2 size) {
    return (floor(t) + 0.5) / size;
}

// No implicit derivatives anywhere in this shader. Every sample below is `textureLod` or
// `textureGrad`: the letterbox/outline tests above the samples are per-pixel branches, and a
// plain `texture()` inside one takes its derivatives from the 2×2 quad's other lanes — which, in
// a quad straddling the image boundary, took the other branch and hold whatever their registers
// last held. That is a garbage mip level on the boundary pixels, and since the garbage is
// whatever the previous draw left behind, a flickering one. The screen→image mapping is a pure
// uniform scale, so the true gradient is a constant: `inv_zoom` texels per screen pixel on each
// axis, and the LOD is `log2(inv_zoom)`. Passing that is both exact and immune to lane divergence.
vec2 grad_x(vec2 size) { return vec2(inv_zoom / size.x, 0.0); }
vec2 grad_y(vec2 size) { return vec2(0.0, inv_zoom / size.y); }

// Sample the flipbook frame texel `f` (frame-local, 0..img_size) from the sheet cell at origin
// `cell`. Explicit-LOD: the sheet's mip chain averages across cell boundaries, so free-running
// mips would ghost neighbouring frames into a minified frame — clamp to `fb_max_lod`. A half-texel
// inset keeps bilinear taps inside the cell; magnify (inv_zoom<=1) stays crisp at mip 0.
vec4 sample_cell(vec2 f, vec2 cell) {
    vec2 t  = cell + clamp(f, vec2(0.5), img_size - 0.5);
    vec2 uv = t / sheet_size;
    vec4 s;
    if (inv_zoom <= 1.0) {
        s = textureLod(sampler2D(tex, samp_point), texel_center(t, sheet_size), 0.0);
    } else {
        float lod = min(log2(inv_zoom), fb_max_lod);
        s = textureLod(sampler2D(tex, samp_aniso), uv, lod);
    }
    if (linear_sample == 0) s.rgb = srgb_to_linear(s.rgb);
    return s;
}

// The whole pipeline, in linear light. `main` encodes what this returns.
vec4 shade(vec4 pos) {
    if (has_image == 0) return clear_lin;
    // gl_FragCoord is in RENDER-TARGET space, not viewport space: the viewport transform is
    // applied before the fragment stage, so a viewport parked below the toolbar still hands us
    // absolute client coordinates. Subtracting the sub-rect's origin puts us back in the
    // viewport's own frame, which is what every line below (centering, the outline, the
    // checkerboard) assumes. Skip it and the image opens `toolbar_h` px too high, top clipped.
    vec2 sp = pos.xy - surf_origin;         // viewport pixel center (origin top-left)
    vec2 ctr = surf_size * 0.5 + pan;
    vec2 f = img_size * 0.5 + (sp - ctr) * inv_zoom;   // image texel coords
    // A 1px (screen-space) outline hugging the OUTSIDE of the image boundary, drawn in the
    // letterbox gutter so it never covers image content. `sd` is the box signed distance in
    // texels (>0 outside the image, <0 inside), `sd_px` the same in screen pixels: the outline
    // is the ring of pixels whose centers land within one screen pixel outside the boundary.
    // White on a black backdrop, else black, so it always contrasts.
    //
    // The window is biased inward by EPS to kill a degenerate case. A naive (0, 1) window is one
    // pixel wide with both ends open, so when an edge lands exactly on a column/row of pixel
    // centers — surf_size and img_size*zoom of opposite parity at pan 0, i.e. routinely — the
    // pixel inside it has sd_px == 0 and its outer neighbour sd_px == 1, neither passes, and that
    // whole edge vanishes until a pan/zoom nudge breaks the tie. Biasing makes the on-boundary
    // pixel (half outside the image anyway) the outline pixel there. Any unit-length half-open
    // window catches exactly one center per row/column, so nothing else changes.
    const float EPS = 1.0 / 256.0;
    float sd = max(max(-f.x, f.x - img_size.x), max(-f.y, f.y - img_size.y));
    float sd_px = sd / inv_zoom;
    if (outline != 0 && sd_px > -EPS && sd_px < 1.0 - EPS) {
        float v = (background == 0) ? 1.0 : 0.0;
        return vec4(v, v, v, 1.0);
    }
    if (f.x < 0.0 || f.y < 0.0 || f.x >= img_size.x || f.y >= img_size.y)
        return vec4(backdrop(sp), 1.0);                 // letterbox = chosen backdrop (frame rect)
    vec3 rgb;
    float a;
    if (fb_on != 0) {
        // Flipbook: sample frame A of the sheet, crossfading toward frame B (sample_cell decodes).
        vec4 s = sample_cell(f, cell_a);
        if (fb_blend > 0.0) s = mix(s, sample_cell(f, cell_b), fb_blend);
        rgb = s.rgb;
        a = s.a;
    } else {
        // f is inside [0, img_size) here (the letterbox branch above returned), so the point tap's
        // floor lands on a real texel and the CLAMP address mode never comes into it.
        vec4 s = (inv_zoom <= 1.0)
            ? textureLod(sampler2D(tex, samp_point), texel_center(f, img_size), 0.0) // magnify
            : textureGrad(sampler2D(tex, samp_aniso), f / img_size,                  // minify
                          grad_x(img_size), grad_y(img_size));
        rgb = s.rgb;
        a = s.a;
        if (linear_sample == 0) rgb = srgb_to_linear(rgb);
    }
    if (is_hdr != 0) {
        rgb *= exposure;
        rgb = (tonemap == 1) ? aces(rgb) : reinhard(rgb);
    }
    vec3 outc;
    if (channel == 1) outc = rgb.rrr;
    else if (channel == 2) outc = rgb.ggg;
    else if (channel == 3) outc = rgb.bbb;
    else if (channel == 4) outc = srgb_to_linear(vec3(a)).xxx;
    // 5 = RGB: the color values on their own, as if the image were opaque. No composite, so a
    // transparent region shows whatever color it actually carries instead of the backdrop.
    else if (channel == 5) outc = rgb;
    else {                                                   // 0 = RGBA (alpha composited)
        outc = rgb;
        if (a < 0.999) outc = backdrop(sp) * (1.0 - a) + rgb * a;
    }
    // Octagon overlay "hide outside": fade pixels outside the octagon inscribed in the frame rect
    // toward the backdrop. Unity's octagon (see crate::octagon): midpoint vertices pinned at
    // (±0.5, 0)/(0, ±0.5), corner vertices at (±a, ±a) with a = 0.5·(1−crop). Each side's
    // half-plane reduces to q.x + k·q.y ≤ 0.5 (and its mirror) with q the |offset| from the frame
    // center and k = crop/(1−crop): k=0 is the quad, k=1 the diamond. In flipbook mode
    // `f`/`img_size` are already the frame rect, so the shape tracks the shown cell.
    if (oct_hide > 0.0) {
        vec2 q = abs(f / img_size - 0.5);
        float k = oct_crop / max(1.0 - oct_crop, 0.5);
        if (max(q.x + k * q.y, q.y + k * q.x) > 0.5)
            outc = mix(outc, backdrop(sp), oct_hide);
    }
    return vec4(outc, 1.0);
}

void main() {
    vec4 c = shade(gl_FragCoord);
    frag_color = vec4(linear_to_srgb(clamp(c.rgb, 0.0, 1.0)), c.a);
}
@end

@program viewport vs_main ps_main
