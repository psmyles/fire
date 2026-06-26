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
    float2 img_size;
    float2 surf_size;
    float2 pan;
    float  inv_zoom;
    float  exposure;
    int    channel;        // 0=RGB 1=R 2=G 3=B 4=A
    int    tonemap;        // 0=Reinhard 1=ACES
    int    is_hdr;
    int    has_image;
    int    linear_sample;  // 1=sample already linear, 0=sRGB-decode rgb in shader
    int3   _pad;
    float4 clear_lin;
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
float3 aces(float3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return saturate((x * (a * x + b)) / (x * (c * x + d) + e));
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    if (has_image == 0) return clear_lin;
    float2 sp = pos.xy;                       // surface pixel center (origin top-left)
    float2 ctr = surf_size * 0.5 + pan;
    float2 f = img_size * 0.5 + (sp - ctr) * inv_zoom;   // image texel coords
    if (f.x < 0.0 || f.y < 0.0 || f.x >= img_size.x || f.y >= img_size.y)
        return clear_lin;
    float2 uv = f / img_size;
    float4 s = (inv_zoom <= 1.0) ? tex.Sample(samp_point, uv)   // magnify/1:1 -> crisp texels
                                 : tex.Sample(samp_aniso, uv);  // minify -> mips + anisotropic
    float3 rgb = s.rgb;
    float a = s.a;
    if (linear_sample == 0) rgb = srgb_to_linear(rgb);
    if (is_hdr != 0) {
        rgb *= exposure;
        rgb = (tonemap == 1) ? aces(rgb) : reinhard(rgb);
    }
    if (channel == 1) return float4(rgb.rrr, 1.0);
    if (channel == 2) return float4(rgb.ggg, 1.0);
    if (channel == 3) return float4(rgb.bbb, 1.0);
    if (channel == 4) { float v = srgb_to_linear(float3(a, a, a)).x; return float4(v, v, v, 1.0); }
    if (a < 0.999) {
        float2 cell = floor(sp / 12.0);
        float bg = (fmod(cell.x + cell.y, 2.0) < 0.5) ? 0.45 : 0.21;
        rgb = bg * (1.0 - a) + rgb * a;
    }
    return float4(rgb, 1.0);
}
