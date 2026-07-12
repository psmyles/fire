// Fire viewport shader: a fullscreen-triangle vertex shader (from SV_VertexID) plus a pixel
// shader that is a direct port of the former CPU per-pixel pipeline: inverse-map the surface
// pixel into image space, sample (point when magnifying for crisp texels, anisotropic+mips when
// minifying), then exposure -> tonemap -> channel isolation -> checker composite, all in linear
// light. The *_SRGB render target handles the final sRGB encode.
//
// Precompiled to DXBC at build time by `fxc` (see build.rs) into vs_main.dxbc / ps_main.dxbc,
// which gpu.rs embeds via include_bytes!. There is no runtime HLSL compile.
//
// The `cbuffer` layout must stay in lockstep with the `Params` struct in gpu.rs (16-byte
// float4 registers, same field order/padding).

Texture2D tex : register(t0);
SamplerState samp_aniso : register(s0);
SamplerState samp_point : register(s1);

cbuffer Params : register(b0) {
    float2 img_size;       // frame rect in flipbook mode (fb_on), else whole image
    float2 surf_size;
    float2 pan;
    float  inv_zoom;
    float  exposure;
    int    channel;        // 0=RGB 1=R 2=G 3=B 4=A
    int    tonemap;        // 0=Reinhard 1=ACES
    int    is_hdr;
    int    has_image;
    int    linear_sample;  // 1=sample already linear, 0=sRGB-decode rgb in shader
    int    background;     // 0=black 1=white 2=grey 3=checker (letterbox + transparency)
    int    outline;        // 1=draw a 1px image-boundary outline
    int    fb_on;          // 1=flipbook: img_size is a cell rect, sample cell_a/cell_b of the sheet
    float4 clear_lin;
    float2 sheet_size;     // whole texture (texels); flipbook cell offsets are in this space
    float2 cell_a;         // frame-A cell origin (texels)
    float2 cell_b;         // frame-B cell origin (== cell_a when not blending)
    float  fb_blend;       // 0..1 crossfade toward frame B (0 = hard cut)
    float  fb_max_lod;     // mip clamp so minified samples can't bleed across cells
    float2 surf_origin;    // image sub-rect's top-left in RENDER-TARGET px (see ps_main)
};

struct VSOut { float4 pos : SV_Position; };

VSOut vs_main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2); // (0,0) (2,0) (0,2)
    VSOut o;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float3 srgb_to_linear(float3 c) {
    float3 lo = c / 12.92;
    float3 hi = pow(max((c + 0.055) / 1.055, 0.0), 2.4);
    return lerp(hi, lo, step(c, 0.04045));
}
float3 reinhard(float3 c) { return c / (1.0 + c); }

// The viewport backdrop in linear light: solid black/white/40%-grey, or a Photoshop-style
// checkerboard keyed to the surface pixel. Used both for the letterbox around the image and as
// the composite behind transparent pixels, so a partly-transparent image reads consistently.
float3 backdrop(float2 sp) {
    if (background == 0) return float3(0.0, 0.0, 0.0);
    if (background == 1) return float3(1.0, 1.0, 1.0);
    if (background == 2) return srgb_to_linear(float3(0.4, 0.4, 0.4));
    float2 cell = floor(sp / 12.0);
    float v = (fmod(cell.x + cell.y, 2.0) < 0.5) ? 0.45 : 0.21; // light/dark checker (linear)
    return float3(v, v, v);
}
float3 aces(float3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return saturate((x * (a * x + b)) / (x * (c * x + d) + e));
}

// Sample the flipbook frame texel `f` (frame-local, 0..img_size) from the sheet cell at origin
// `cell`. Explicit-LOD: the sheet's mip chain averages across cell boundaries, so implicit mips
// would ghost neighbouring frames into a minified frame — clamp to `fb_max_lod`. A half-texel
// inset keeps bilinear/aniso taps inside the cell; magnify (inv_zoom<=1) stays crisp at mip 0.
float4 sample_cell(float2 f, float2 cell) {
    float2 t  = cell + clamp(f, 0.5, img_size - 0.5);
    float2 uv = t / sheet_size;
    float4 s;
    if (inv_zoom <= 1.0) {
        s = tex.SampleLevel(samp_point, uv, 0.0);
    } else {
        float lod = min(tex.CalculateLevelOfDetail(samp_aniso, uv), fb_max_lod);
        s = tex.SampleLevel(samp_aniso, uv, lod);
    }
    if (linear_sample == 0) s.rgb = srgb_to_linear(s.rgb);
    return s;
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    if (has_image == 0) return clear_lin;
    // SV_Position is in RENDER-TARGET space, not viewport space: D3D applies the viewport transform
    // before the fragment stage, so a viewport parked below the toolbar still hands us absolute
    // client coordinates. Subtracting the sub-rect's origin puts us back in the viewport's own frame,
    // which is what every line below (centering, the outline, the checkerboard) assumes. Skip it and
    // the image opens `toolbar_h` px too high, with its top clipped off.
    float2 sp = pos.xy - surf_origin;         // viewport pixel center (origin top-left)
    float2 ctr = surf_size * 0.5 + pan;
    float2 f = img_size * 0.5 + (sp - ctr) * inv_zoom;   // image texel coords
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
        return float4(v, v, v, 1.0);
    }
    if (f.x < 0.0 || f.y < 0.0 || f.x >= img_size.x || f.y >= img_size.y)
        return float4(backdrop(sp), 1.0);                // letterbox = chosen backdrop (frame rect)
    float3 rgb;
    float a;
    if (fb_on != 0) {
        // Flipbook: sample frame A of the sheet, crossfading toward frame B (sample_cell decodes).
        float4 s = sample_cell(f, cell_a);
        if (fb_blend > 0.0) s = lerp(s, sample_cell(f, cell_b), fb_blend);
        rgb = s.rgb;
        a = s.a;
    } else {
        float2 uv = f / img_size;
        float4 s = (inv_zoom <= 1.0) ? tex.Sample(samp_point, uv)   // magnify/1:1 -> crisp texels
                                     : tex.Sample(samp_aniso, uv);  // minify -> mips + anisotropic
        rgb = s.rgb;
        a = s.a;
        if (linear_sample == 0) rgb = srgb_to_linear(rgb);
    }
    if (is_hdr != 0) {
        rgb *= exposure;
        rgb = (tonemap == 1) ? aces(rgb) : reinhard(rgb);
    }
    if (channel == 1) return float4(rgb.rrr, 1.0);
    if (channel == 2) return float4(rgb.ggg, 1.0);
    if (channel == 3) return float4(rgb.bbb, 1.0);
    if (channel == 4) { float v = srgb_to_linear(float3(a, a, a)).x; return float4(v, v, v, 1.0); }
    if (a < 0.999) rgb = backdrop(sp) * (1.0 - a) + rgb * a;
    return float4(rgb, 1.0);
}
