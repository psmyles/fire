#pragma clang diagnostic ignored "-Wmissing-prototypes"

#include <metal_stdlib>
#include <simd/simd.h>

using namespace metal;

// Implementation of the GLSL mod() function, which is slightly different than Metal fmod()
template<typename Tx, typename Ty>
inline Tx mod(Tx x, Ty y)
{
    return x - y * floor(x / y);
}

struct params
{
    float2 img_size;
    float2 surf_size;
    float2 pan;
    float inv_zoom;
    float exposure;
    int channel;
    int tonemap;
    int is_hdr;
    int has_image;
    int linear_sample;
    int background;
    int outline;
    int fb_on;
    float4 clear_lin;
    float2 sheet_size;
    float2 cell_a;
    float2 cell_b;
    float fb_blend;
    float fb_max_lod;
    float2 surf_origin;
    float oct_crop;
    float oct_hide;
};

struct main0_out
{
    float4 frag_color [[color(0)]];
};

static inline __attribute__((always_inline))
float3 srgb_to_linear(thread const float3& c)
{
    return mix(powr(fast::max((c + float3(0.054999999701976776123046875)) * float3(0.947867333889007568359375), float3(0.0)), float3(2.400000095367431640625)), c * float3(0.077399380505084991455078125), step(c, float3(0.040449999272823333740234375)));
}

static inline __attribute__((always_inline))
float3 backdrop(thread const float2& sp, constant params& _112)
{
    if (_112.background == 0)
    {
        return float3(0.0);
    }
    if (_112.background == 1)
    {
        return float3(1.0);
    }
    if (_112.background == 2)
    {
        float3 param = float3(0.4000000059604644775390625);
        return srgb_to_linear(param);
    }
    float2 _148 = floor(sp * float2(0.083333335816860198974609375));
    return float3((mod(_148.x + _148.y, 2.0) < 0.5) ? 0.449999988079071044921875 : 0.20999999344348907470703125);
}

static inline __attribute__((always_inline))
float2 texel_center(thread const float2& t, thread const float2& size)
{
    return (floor(t) + float2(0.5)) / size;
}

static inline __attribute__((always_inline))
float4 sample_cell(thread const float2& f, thread const float2& cell, constant params& _112, texture2d<float> tex, sampler samp_point, sampler samp_aniso)
{
    float2 _231 = cell + fast::clamp(f, float2(0.5), _112.img_size - float2(0.5));
    float4 s;
    if (_112.inv_zoom <= 1.0)
    {
        float2 param = _231;
        float2 param_1 = _112.sheet_size;
        s = tex.sample(samp_point, texel_center(param, param_1), level(0.0));
    }
    else
    {
        s = tex.sample(samp_aniso, (_231 / _112.sheet_size), level(fast::min(log2(_112.inv_zoom), _112.fb_max_lod)));
    }
    if (_112.linear_sample == 0)
    {
        float3 param_2 = s.xyz;
        float3 _286 = srgb_to_linear(param_2);
        float4 _671 = s;
        _671.x = _286.x;
        _671.y = _286.y;
        _671.z = _286.z;
        s = _671;
    }
    return s;
}

static inline __attribute__((always_inline))
float2 grad_x(thread const float2& size, constant params& _112)
{
    return float2(_112.inv_zoom / size.x, 0.0);
}

static inline __attribute__((always_inline))
float2 grad_y(thread const float2& size, constant params& _112)
{
    return float2(0.0, _112.inv_zoom / size.y);
}

static inline __attribute__((always_inline))
float3 aces(thread const float3& x)
{
    return fast::clamp((x * ((x * 2.5099999904632568359375) + float3(0.02999999932944774627685546875))) / ((x * ((x * 2.4300000667572021484375) + float3(0.589999973773956298828125))) + float3(0.14000000059604644775390625)), float3(0.0), float3(1.0));
}

static inline __attribute__((always_inline))
float3 reinhard(thread const float3& c)
{
    return c / (float3(1.0) + c);
}

static inline __attribute__((always_inline))
float4 shade(thread const float4& pos, constant params& _112, texture2d<float> tex, sampler samp_point, sampler samp_aniso)
{
    if (_112.has_image == 0)
    {
        return _112.clear_lin;
    }
    float2 _314 = pos.xy - _112.surf_origin;
    float2 _332 = (_112.img_size * 0.5) + ((_314 - ((_112.surf_size * 0.5) + _112.pan)) * _112.inv_zoom);
    float _335 = _332.x;
    float _344 = _332.y;
    float _357 = fast::max(fast::max(-_335, _335 - _112.img_size.x), fast::max(-_344, _344 - _112.img_size.y)) / _112.inv_zoom;
    if (((_112.outline != 0) && (_357 > (-0.00390625))) && (_357 < 0.99609375))
    {
        float _376 = float(_112.background == 0);
        return float4(_376, _376, _376, 1.0);
    }
    bool _382 = _335 < 0.0;
    bool _389;
    if (!_382)
    {
        _389 = _344 < 0.0;
    }
    else
    {
        _389 = _382;
    }
    bool _398;
    if (!_389)
    {
        _398 = _335 >= _112.img_size.x;
    }
    else
    {
        _398 = _389;
    }
    bool _407;
    if (!_398)
    {
        _407 = _344 >= _112.img_size.y;
    }
    else
    {
        _407 = _398;
    }
    if (_407)
    {
        float2 param = _314;
        return float4(backdrop(param, _112), 1.0);
    }
    float3 rgb;
    float a;
    if (_112.fb_on != 0)
    {
        float2 param_1 = _332;
        float2 param_2 = _112.cell_a;
        float4 s = sample_cell(param_1, param_2, _112, tex, samp_point, samp_aniso);
        if (_112.fb_blend > 0.0)
        {
            float2 param_3 = _332;
            float2 param_4 = _112.cell_b;
            s = mix(s, sample_cell(param_3, param_4, _112, tex, samp_point, samp_aniso), float4(_112.fb_blend));
        }
        rgb = s.xyz;
        a = s.w;
    }
    else
    {
        float4 _462;
        if (_112.inv_zoom <= 1.0)
        {
            float2 param_5 = _332;
            float2 param_6 = _112.img_size;
            _462 = tex.sample(samp_point, texel_center(param_5, param_6), level(0.0));
        }
        else
        {
            float2 param_7 = _112.img_size;
            float2 param_8 = _112.img_size;
            _462 = tex.sample(samp_aniso, (_332 / _112.img_size), gradient2d(grad_x(param_7, _112), grad_y(param_8, _112)));
        }
        rgb = _462.xyz;
        a = _462.w;
        if (_112.linear_sample == 0)
        {
            float3 param_9 = rgb;
            rgb = srgb_to_linear(param_9);
        }
    }
    if (_112.is_hdr != 0)
    {
        rgb *= _112.exposure;
        float3 _520;
        if (_112.tonemap == 1)
        {
            float3 param_10 = rgb;
            _520 = aces(param_10);
        }
        else
        {
            float3 param_11 = rgb;
            _520 = reinhard(param_11);
        }
        rgb = _520;
    }
    float3 outc;
    if (_112.channel == 1)
    {
        outc = rgb.xxx;
    }
    else
    {
        if (_112.channel == 2)
        {
            outc = rgb.yyy;
        }
        else
        {
            if (_112.channel == 3)
            {
                outc = rgb.zzz;
            }
            else
            {
                if (_112.channel == 4)
                {
                    float3 param_12 = float3(a);
                    outc = srgb_to_linear(param_12).xxx;
                }
                else
                {
                    if (_112.channel == 5)
                    {
                        outc = rgb;
                    }
                    else
                    {
                        outc = rgb;
                        if (a < 0.999000012874603271484375)
                        {
                            float2 param_13 = _314;
                            outc = (backdrop(param_13, _112) * (1.0 - a)) + (rgb * a);
                        }
                    }
                }
            }
        }
    }
    if (_112.oct_hide > 0.0)
    {
        float2 _604 = abs((_332 / _112.img_size) - float2(0.5));
        float _613 = _112.oct_crop / fast::max(1.0 - _112.oct_crop, 0.5);
        float _615 = _604.x;
        float _618 = _604.y;
        if (fast::max(_615 + (_613 * _618), _618 + (_613 * _615)) > 0.5)
        {
            float2 param_14 = _314;
            outc = mix(outc, backdrop(param_14, _112), float3(_112.oct_hide));
        }
    }
    return float4(outc, 1.0);
}

static inline __attribute__((always_inline))
float3 linear_to_srgb(thread const float3& c)
{
    return mix((powr(fast::max(c, float3(0.0)), float3(0.4166666567325592041015625)) * 1.05499994754791259765625) - float3(0.054999999701976776123046875), c * 12.9200000762939453125, step(c, float3(0.003130800090730190277099609375)));
}

fragment main0_out main0(constant params& _112 [[buffer(0)]], texture2d<float> tex [[texture(0)]], sampler samp_aniso [[sampler(0)]], sampler samp_point [[sampler(1)]], float4 gl_FragCoord [[position]])
{
    main0_out out = {};
    float4 param = gl_FragCoord;
    float4 _652 = shade(param, _112, tex, samp_point, samp_aniso);
    float3 param_1 = fast::clamp(_652.xyz, float3(0.0), float3(1.0));
    out.frag_color = float4(linear_to_srgb(param_1), _652.w);
    return out;
}

