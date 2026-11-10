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
    float4 layer_rect;
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
float3 backdrop(thread const float2& sp, constant params& _115)
{
    if (_115.background == 0)
    {
        return float3(0.0);
    }
    if (_115.background == 1)
    {
        return float3(1.0);
    }
    if (_115.background == 2)
    {
        float3 param = float3(0.4000000059604644775390625);
        return srgb_to_linear(param);
    }
    float2 _151 = floor(sp * float2(0.083333335816860198974609375));
    return float3((mod(_151.x + _151.y, 2.0) < 0.5) ? 0.449999988079071044921875 : 0.20999999344348907470703125);
}

static inline __attribute__((always_inline))
float2 texel_center(thread const float2& t, thread const float2& size)
{
    return (floor(t) + float2(0.5)) / size;
}

static inline __attribute__((always_inline))
float4 sample_cell(thread const float2& f, thread const float2& cell, constant params& _115, texture2d<float> tex, sampler samp_point, sampler samp_aniso)
{
    float2 _237 = cell + fast::clamp(f, float2(0.5), _115.img_size - float2(0.5));
    float4 s;
    if (_115.inv_zoom <= 1.0)
    {
        float2 param = _237;
        float2 param_1 = _115.sheet_size;
        s = tex.sample(samp_point, texel_center(param, param_1), level(0.0));
    }
    else
    {
        s = tex.sample(samp_aniso, (_237 / _115.sheet_size), level(fast::min(log2(_115.inv_zoom), _115.fb_max_lod)));
    }
    if (_115.linear_sample == 0)
    {
        float3 param_2 = s.xyz;
        float3 _292 = srgb_to_linear(param_2);
        float4 _715 = s;
        _715.x = _292.x;
        _715.y = _292.y;
        _715.z = _292.z;
        s = _715;
    }
    return s;
}

static inline __attribute__((always_inline))
float2 grad_x(thread const float2& size, constant params& _115)
{
    return float2(_115.inv_zoom / size.x, 0.0);
}

static inline __attribute__((always_inline))
float2 grad_y(thread const float2& size, constant params& _115)
{
    return float2(0.0, _115.inv_zoom / size.y);
}

static inline __attribute__((always_inline))
float3 aces(thread float3& x)
{
    x = fast::max(x, float3(0.0));
    return fast::clamp((x * ((x * 2.5099999904632568359375) + float3(0.02999999932944774627685546875))) / ((x * ((x * 2.4300000667572021484375) + float3(0.589999973773956298828125))) + float3(0.14000000059604644775390625)), float3(0.0), float3(1.0));
}

static inline __attribute__((always_inline))
float3 reinhard(thread float3& c)
{
    c = fast::max(c, float3(0.0));
    return c / (float3(1.0) + c);
}

static inline __attribute__((always_inline))
float4 shade(thread const float4& pos, constant params& _115, texture2d<float> tex, sampler samp_point, sampler samp_aniso)
{
    if (_115.has_image == 0)
    {
        return _115.clear_lin;
    }
    float2 _320 = pos.xy - _115.surf_origin;
    float2 _338 = (_115.img_size * 0.5) + ((_320 - ((_115.surf_size * 0.5) + _115.pan)) * _115.inv_zoom);
    float _341 = _338.x;
    float _350 = _338.y;
    float _363 = fast::max(fast::max(-_341, _341 - _115.img_size.x), fast::max(-_350, _350 - _115.img_size.y)) / _115.inv_zoom;
    if (((_115.outline != 0) && (_363 > (-0.00390625))) && (_363 < 0.99609375))
    {
        float _382 = float(_115.background == 0);
        return float4(_382, _382, _382, 1.0);
    }
    bool _388 = _341 < 0.0;
    bool _395;
    if (!_388)
    {
        _395 = _350 < 0.0;
    }
    else
    {
        _395 = _388;
    }
    bool _404;
    if (!_395)
    {
        _404 = _341 >= _115.img_size.x;
    }
    else
    {
        _404 = _395;
    }
    bool _413;
    if (!_404)
    {
        _413 = _350 >= _115.img_size.y;
    }
    else
    {
        _413 = _404;
    }
    if (_413)
    {
        float2 param = _320;
        return float4(backdrop(param, _115), 1.0);
    }
    float3 rgb;
    float a;
    if (_115.fb_on != 0)
    {
        float2 param_1 = _338;
        float2 param_2 = _115.cell_a;
        float4 s = sample_cell(param_1, param_2, _115, tex, samp_point, samp_aniso);
        if (_115.fb_blend > 0.0)
        {
            float2 param_3 = _338;
            float2 param_4 = _115.cell_b;
            s = mix(s, sample_cell(param_3, param_4, _115, tex, samp_point, samp_aniso), float4(_115.fb_blend));
        }
        rgb = s.xyz;
        a = s.w;
    }
    else
    {
        float2 _470 = _338 - _115.layer_rect.xy;
        float _476 = _470.x;
        bool _477 = _476 < 0.0;
        bool _484;
        if (!_477)
        {
            _484 = _470.y < 0.0;
        }
        else
        {
            _484 = _477;
        }
        bool _493;
        if (!_484)
        {
            _493 = _476 >= _115.layer_rect.z;
        }
        else
        {
            _493 = _484;
        }
        bool _502;
        if (!_493)
        {
            _502 = _470.y >= _115.layer_rect.w;
        }
        else
        {
            _502 = _493;
        }
        if (_502)
        {
            rgb = float3(0.0);
            a = 0.0;
        }
        else
        {
            float4 _510;
            if (_115.inv_zoom <= 1.0)
            {
                float2 param_5 = _470;
                float2 param_6 = _115.layer_rect.zw;
                _510 = tex.sample(samp_point, texel_center(param_5, param_6), level(0.0));
            }
            else
            {
                float2 param_7 = _115.layer_rect.zw;
                float2 param_8 = _115.layer_rect.zw;
                _510 = tex.sample(samp_aniso, (_470 / _115.layer_rect.zw), gradient2d(grad_x(param_7, _115), grad_y(param_8, _115)));
            }
            rgb = _510.xyz;
            a = _510.w;
            if (_115.linear_sample == 0)
            {
                float3 param_9 = rgb;
                rgb = srgb_to_linear(param_9);
            }
        }
    }
    if (_115.is_hdr != 0)
    {
        rgb *= _115.exposure;
        float3 _564;
        if (_115.tonemap == 1)
        {
            float3 param_10 = rgb;
            float3 _569 = aces(param_10);
            _564 = _569;
        }
        else
        {
            float3 param_11 = rgb;
            float3 _573 = reinhard(param_11);
            _564 = _573;
        }
        rgb = _564;
    }
    float3 outc;
    if (_115.channel == 1)
    {
        outc = rgb.xxx;
    }
    else
    {
        if (_115.channel == 2)
        {
            outc = rgb.yyy;
        }
        else
        {
            if (_115.channel == 3)
            {
                outc = rgb.zzz;
            }
            else
            {
                if (_115.channel == 4)
                {
                    float3 param_12 = float3(a);
                    outc = srgb_to_linear(param_12).xxx;
                }
                else
                {
                    if (_115.channel == 5)
                    {
                        outc = rgb;
                    }
                    else
                    {
                        outc = rgb;
                        if (a < 0.999000012874603271484375)
                        {
                            float2 param_13 = _320;
                            outc = (backdrop(param_13, _115) * (1.0 - a)) + (rgb * a);
                        }
                    }
                }
            }
        }
    }
    if (_115.oct_hide > 0.0)
    {
        float2 _648 = abs((_338 / _115.img_size) - float2(0.5));
        float _657 = _115.oct_crop / fast::max(1.0 - _115.oct_crop, 0.5);
        float _659 = _648.x;
        float _662 = _648.y;
        if (fast::max(_659 + (_657 * _662), _662 + (_657 * _659)) > 0.5)
        {
            float2 param_14 = _320;
            outc = mix(outc, backdrop(param_14, _115), float3(_115.oct_hide));
        }
    }
    return float4(outc, 1.0);
}

static inline __attribute__((always_inline))
float3 linear_to_srgb(thread const float3& c)
{
    return mix((powr(fast::max(c, float3(0.0)), float3(0.4166666567325592041015625)) * 1.05499994754791259765625) - float3(0.054999999701976776123046875), c * 12.9200000762939453125, step(c, float3(0.003130800090730190277099609375)));
}

fragment main0_out main0(constant params& _115 [[buffer(0)]], texture2d<float> tex [[texture(0)]], sampler samp_aniso [[sampler(0)]], sampler samp_point [[sampler(1)]], float4 gl_FragCoord [[position]])
{
    main0_out out = {};
    float4 param = gl_FragCoord;
    float4 _696 = shade(param, _115, tex, samp_point, samp_aniso);
    float3 param_1 = fast::clamp(_696.xyz, float3(0.0), float3(1.0));
    out.frag_color = float4(linear_to_srgb(param_1), _696.w);
    return out;
}

