cbuffer params : register(b0)
{
    float2 _112_img_size : packoffset(c0);
    float2 _112_surf_size : packoffset(c0.z);
    float2 _112_pan : packoffset(c1);
    float _112_inv_zoom : packoffset(c1.z);
    float _112_exposure : packoffset(c1.w);
    int _112_channel : packoffset(c2);
    int _112_tonemap : packoffset(c2.y);
    int _112_is_hdr : packoffset(c2.z);
    int _112_has_image : packoffset(c2.w);
    int _112_linear_sample : packoffset(c3);
    int _112_background : packoffset(c3.y);
    int _112_outline : packoffset(c3.z);
    int _112_fb_on : packoffset(c3.w);
    float4 _112_clear_lin : packoffset(c4);
    float2 _112_sheet_size : packoffset(c5);
    float2 _112_cell_a : packoffset(c5.z);
    float2 _112_cell_b : packoffset(c6);
    float _112_fb_blend : packoffset(c6.z);
    float _112_fb_max_lod : packoffset(c6.w);
    float2 _112_surf_origin : packoffset(c7);
    float _112_oct_crop : packoffset(c7.z);
    float _112_oct_hide : packoffset(c7.w);
};

Texture2D<float4> tex : register(t0);
SamplerState samp_point : register(s1);
SamplerState samp_aniso : register(s0);

static float4 gl_FragCoord;
static float4 frag_color;

struct SPIRV_Cross_Input
{
    float4 gl_FragCoord : SV_Position;
};

struct SPIRV_Cross_Output
{
    float4 frag_color : SV_Target0;
};

float mod(float x, float y)
{
    return x - y * floor(x / y);
}

float2 mod(float2 x, float2 y)
{
    return x - y * floor(x / y);
}

float3 mod(float3 x, float3 y)
{
    return x - y * floor(x / y);
}

float4 mod(float4 x, float4 y)
{
    return x - y * floor(x / y);
}

float3 srgb_to_linear(float3 c)
{
    return lerp(pow(max((c + 0.054999999701976776123046875f.xxx) * 0.947867333889007568359375f.xxx, 0.0f.xxx), 2.400000095367431640625f.xxx), c * 0.077399380505084991455078125f.xxx, step(c, 0.040449999272823333740234375f.xxx));
}

float3 backdrop(float2 sp)
{
    if (_112_background == 0)
    {
        return 0.0f.xxx;
    }
    if (_112_background == 1)
    {
        return 1.0f.xxx;
    }
    if (_112_background == 2)
    {
        float3 param = 0.4000000059604644775390625f.xxx;
        return srgb_to_linear(param);
    }
    float2 _148 = floor(sp * 0.083333335816860198974609375f.xx);
    return ((mod(_148.x + _148.y, 2.0f) < 0.5f) ? 0.449999988079071044921875f : 0.20999999344348907470703125f).xxx;
}

float2 texel_center(float2 t, float2 size)
{
    return (floor(t) + 0.5f.xx) / size;
}

float4 sample_cell(float2 f, float2 cell)
{
    float2 _231 = cell + clamp(f, 0.5f.xx, _112_img_size - 0.5f.xx);
    float4 s;
    if (_112_inv_zoom <= 1.0f)
    {
        float2 param = _231;
        float2 param_1 = _112_sheet_size;
        s = tex.SampleLevel(samp_point, texel_center(param, param_1), 0.0f);
    }
    else
    {
        s = tex.SampleLevel(samp_aniso, _231 / _112_sheet_size, min(log2(_112_inv_zoom), _112_fb_max_lod));
    }
    if (_112_linear_sample == 0)
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

float2 grad_x(float2 size)
{
    return float2(_112_inv_zoom / size.x, 0.0f);
}

float2 grad_y(float2 size)
{
    return float2(0.0f, _112_inv_zoom / size.y);
}

float3 aces(float3 x)
{
    return clamp((x * ((x * 2.5099999904632568359375f) + 0.02999999932944774627685546875f.xxx)) / ((x * ((x * 2.4300000667572021484375f) + 0.589999973773956298828125f.xxx)) + 0.14000000059604644775390625f.xxx), 0.0f.xxx, 1.0f.xxx);
}

float3 reinhard(float3 c)
{
    return c / (1.0f.xxx + c);
}

float4 shade(float4 pos)
{
    if (_112_has_image == 0)
    {
        return _112_clear_lin;
    }
    float2 _314 = pos.xy - _112_surf_origin;
    float2 _332 = (_112_img_size * 0.5f) + ((_314 - ((_112_surf_size * 0.5f) + _112_pan)) * _112_inv_zoom);
    float _335 = _332.x;
    float _344 = _332.y;
    float _357 = max(max(-_335, _335 - _112_img_size.x), max(-_344, _344 - _112_img_size.y)) / _112_inv_zoom;
    if (((_112_outline != 0) && (_357 > (-0.00390625f))) && (_357 < 0.99609375f))
    {
        float _376 = float(_112_background == 0);
        return float4(_376, _376, _376, 1.0f);
    }
    bool _382 = _335 < 0.0f;
    bool _389;
    if (!_382)
    {
        _389 = _344 < 0.0f;
    }
    else
    {
        _389 = _382;
    }
    bool _398;
    if (!_389)
    {
        _398 = _335 >= _112_img_size.x;
    }
    else
    {
        _398 = _389;
    }
    bool _407;
    if (!_398)
    {
        _407 = _344 >= _112_img_size.y;
    }
    else
    {
        _407 = _398;
    }
    if (_407)
    {
        float2 param = _314;
        return float4(backdrop(param), 1.0f);
    }
    float3 rgb;
    float a;
    if (_112_fb_on != 0)
    {
        float2 param_1 = _332;
        float2 param_2 = _112_cell_a;
        float4 s = sample_cell(param_1, param_2);
        if (_112_fb_blend > 0.0f)
        {
            float2 param_3 = _332;
            float2 param_4 = _112_cell_b;
            s = lerp(s, sample_cell(param_3, param_4), _112_fb_blend.xxxx);
        }
        rgb = s.xyz;
        a = s.w;
    }
    else
    {
        float4 _462;
        if (_112_inv_zoom <= 1.0f)
        {
            float2 param_5 = _332;
            float2 param_6 = _112_img_size;
            _462 = tex.SampleLevel(samp_point, texel_center(param_5, param_6), 0.0f);
        }
        else
        {
            float2 param_7 = _112_img_size;
            float2 param_8 = _112_img_size;
            _462 = tex.SampleGrad(samp_aniso, _332 / _112_img_size, grad_x(param_7), grad_y(param_8));
        }
        rgb = _462.xyz;
        a = _462.w;
        if (_112_linear_sample == 0)
        {
            float3 param_9 = rgb;
            rgb = srgb_to_linear(param_9);
        }
    }
    if (_112_is_hdr != 0)
    {
        rgb *= _112_exposure;
        float3 _520;
        if (_112_tonemap == 1)
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
    if (_112_channel == 1)
    {
        outc = rgb.xxx;
    }
    else
    {
        if (_112_channel == 2)
        {
            outc = rgb.yyy;
        }
        else
        {
            if (_112_channel == 3)
            {
                outc = rgb.zzz;
            }
            else
            {
                if (_112_channel == 4)
                {
                    float3 param_12 = a.xxx;
                    outc = srgb_to_linear(param_12).xxx;
                }
                else
                {
                    if (_112_channel == 5)
                    {
                        outc = rgb;
                    }
                    else
                    {
                        outc = rgb;
                        if (a < 0.999000012874603271484375f)
                        {
                            float2 param_13 = _314;
                            outc = (backdrop(param_13) * (1.0f - a)) + (rgb * a);
                        }
                    }
                }
            }
        }
    }
    if (_112_oct_hide > 0.0f)
    {
        float2 _604 = abs((_332 / _112_img_size) - 0.5f.xx);
        float _613 = _112_oct_crop / max(1.0f - _112_oct_crop, 0.5f);
        float _615 = _604.x;
        float _618 = _604.y;
        if (max(_615 + (_613 * _618), _618 + (_613 * _615)) > 0.5f)
        {
            float2 param_14 = _314;
            outc = lerp(outc, backdrop(param_14), _112_oct_hide.xxx);
        }
    }
    return float4(outc, 1.0f);
}

float3 linear_to_srgb(float3 c)
{
    return lerp((pow(max(c, 0.0f.xxx), 0.4166666567325592041015625f.xxx) * 1.05499994754791259765625f) - 0.054999999701976776123046875f.xxx, c * 12.9200000762939453125f, step(c, 0.003130800090730190277099609375f.xxx));
}

void frag_main()
{
    float4 param = gl_FragCoord;
    float4 _652 = shade(param);
    float3 param_1 = clamp(_652.xyz, 0.0f.xxx, 1.0f.xxx);
    frag_color = float4(linear_to_srgb(param_1), _652.w);
}

SPIRV_Cross_Output main(SPIRV_Cross_Input stage_input)
{
    gl_FragCoord = stage_input.gl_FragCoord;
    gl_FragCoord.w = 1.0 / gl_FragCoord.w;
    frag_main();
    SPIRV_Cross_Output stage_output;
    stage_output.frag_color = frag_color;
    return stage_output;
}
