cbuffer params : register(b0)
{
    float2 _115_img_size : packoffset(c0);
    float2 _115_surf_size : packoffset(c0.z);
    float2 _115_pan : packoffset(c1);
    float _115_inv_zoom : packoffset(c1.z);
    float _115_exposure : packoffset(c1.w);
    int _115_channel : packoffset(c2);
    int _115_tonemap : packoffset(c2.y);
    int _115_is_hdr : packoffset(c2.z);
    int _115_has_image : packoffset(c2.w);
    int _115_linear_sample : packoffset(c3);
    int _115_background : packoffset(c3.y);
    int _115_outline : packoffset(c3.z);
    int _115_fb_on : packoffset(c3.w);
    float4 _115_clear_lin : packoffset(c4);
    float2 _115_sheet_size : packoffset(c5);
    float2 _115_cell_a : packoffset(c5.z);
    float2 _115_cell_b : packoffset(c6);
    float _115_fb_blend : packoffset(c6.z);
    float _115_fb_max_lod : packoffset(c6.w);
    float2 _115_surf_origin : packoffset(c7);
    float _115_oct_crop : packoffset(c7.z);
    float _115_oct_hide : packoffset(c7.w);
    float4 _115_layer_rect : packoffset(c8);
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
    if (_115_background == 0)
    {
        return 0.0f.xxx;
    }
    if (_115_background == 1)
    {
        return 1.0f.xxx;
    }
    if (_115_background == 2)
    {
        float3 param = 0.4000000059604644775390625f.xxx;
        return srgb_to_linear(param);
    }
    float2 _151 = floor(sp * 0.083333335816860198974609375f.xx);
    return ((mod(_151.x + _151.y, 2.0f) < 0.5f) ? 0.449999988079071044921875f : 0.20999999344348907470703125f).xxx;
}

float2 texel_center(float2 t, float2 size)
{
    return (floor(t) + 0.5f.xx) / size;
}

float4 sample_cell(float2 f, float2 cell)
{
    float2 _237 = cell + clamp(f, 0.5f.xx, _115_img_size - 0.5f.xx);
    float4 s;
    if (_115_inv_zoom <= 1.0f)
    {
        float2 param = _237;
        float2 param_1 = _115_sheet_size;
        s = tex.SampleLevel(samp_point, texel_center(param, param_1), 0.0f);
    }
    else
    {
        s = tex.SampleLevel(samp_aniso, _237 / _115_sheet_size, min(log2(_115_inv_zoom), _115_fb_max_lod));
    }
    if (_115_linear_sample == 0)
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

float2 grad_x(float2 size)
{
    return float2(_115_inv_zoom / size.x, 0.0f);
}

float2 grad_y(float2 size)
{
    return float2(0.0f, _115_inv_zoom / size.y);
}

float3 aces(inout float3 x)
{
    x = max(x, 0.0f.xxx);
    return clamp((x * ((x * 2.5099999904632568359375f) + 0.02999999932944774627685546875f.xxx)) / ((x * ((x * 2.4300000667572021484375f) + 0.589999973773956298828125f.xxx)) + 0.14000000059604644775390625f.xxx), 0.0f.xxx, 1.0f.xxx);
}

float3 reinhard(inout float3 c)
{
    c = max(c, 0.0f.xxx);
    return c / (1.0f.xxx + c);
}

float4 shade(float4 pos)
{
    if (_115_has_image == 0)
    {
        return _115_clear_lin;
    }
    float2 _320 = pos.xy - _115_surf_origin;
    float2 _338 = (_115_img_size * 0.5f) + ((_320 - ((_115_surf_size * 0.5f) + _115_pan)) * _115_inv_zoom);
    float _341 = _338.x;
    float _350 = _338.y;
    float _363 = max(max(-_341, _341 - _115_img_size.x), max(-_350, _350 - _115_img_size.y)) / _115_inv_zoom;
    if (((_115_outline != 0) && (_363 > (-0.00390625f))) && (_363 < 0.99609375f))
    {
        float _382 = float(_115_background == 0);
        return float4(_382, _382, _382, 1.0f);
    }
    bool _388 = _341 < 0.0f;
    bool _395;
    if (!_388)
    {
        _395 = _350 < 0.0f;
    }
    else
    {
        _395 = _388;
    }
    bool _404;
    if (!_395)
    {
        _404 = _341 >= _115_img_size.x;
    }
    else
    {
        _404 = _395;
    }
    bool _413;
    if (!_404)
    {
        _413 = _350 >= _115_img_size.y;
    }
    else
    {
        _413 = _404;
    }
    if (_413)
    {
        float2 param = _320;
        return float4(backdrop(param), 1.0f);
    }
    float3 rgb;
    float a;
    if (_115_fb_on != 0)
    {
        float2 param_1 = _338;
        float2 param_2 = _115_cell_a;
        float4 s = sample_cell(param_1, param_2);
        if (_115_fb_blend > 0.0f)
        {
            float2 param_3 = _338;
            float2 param_4 = _115_cell_b;
            s = lerp(s, sample_cell(param_3, param_4), _115_fb_blend.xxxx);
        }
        rgb = s.xyz;
        a = s.w;
    }
    else
    {
        float2 _470 = _338 - _115_layer_rect.xy;
        float _476 = _470.x;
        bool _477 = _476 < 0.0f;
        bool _484;
        if (!_477)
        {
            _484 = _470.y < 0.0f;
        }
        else
        {
            _484 = _477;
        }
        bool _493;
        if (!_484)
        {
            _493 = _476 >= _115_layer_rect.z;
        }
        else
        {
            _493 = _484;
        }
        bool _502;
        if (!_493)
        {
            _502 = _470.y >= _115_layer_rect.w;
        }
        else
        {
            _502 = _493;
        }
        if (_502)
        {
            rgb = 0.0f.xxx;
            a = 0.0f;
        }
        else
        {
            float4 _510;
            if (_115_inv_zoom <= 1.0f)
            {
                float2 param_5 = _470;
                float2 param_6 = _115_layer_rect.zw;
                _510 = tex.SampleLevel(samp_point, texel_center(param_5, param_6), 0.0f);
            }
            else
            {
                float2 param_7 = _115_layer_rect.zw;
                float2 param_8 = _115_layer_rect.zw;
                _510 = tex.SampleGrad(samp_aniso, _470 / _115_layer_rect.zw, grad_x(param_7), grad_y(param_8));
            }
            rgb = _510.xyz;
            a = _510.w;
            if (_115_linear_sample == 0)
            {
                float3 param_9 = rgb;
                rgb = srgb_to_linear(param_9);
            }
        }
    }
    if (_115_is_hdr != 0)
    {
        rgb *= _115_exposure;
        float3 _564;
        if (_115_tonemap == 1)
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
    if (_115_channel == 1)
    {
        outc = rgb.xxx;
    }
    else
    {
        if (_115_channel == 2)
        {
            outc = rgb.yyy;
        }
        else
        {
            if (_115_channel == 3)
            {
                outc = rgb.zzz;
            }
            else
            {
                if (_115_channel == 4)
                {
                    float3 param_12 = a.xxx;
                    outc = srgb_to_linear(param_12).xxx;
                }
                else
                {
                    if (_115_channel == 5)
                    {
                        outc = rgb;
                    }
                    else
                    {
                        outc = rgb;
                        if (a < 0.999000012874603271484375f)
                        {
                            float2 param_13 = _320;
                            outc = (backdrop(param_13) * (1.0f - a)) + (rgb * a);
                        }
                    }
                }
            }
        }
    }
    if (_115_oct_hide > 0.0f)
    {
        float2 _648 = abs((_338 / _115_img_size) - 0.5f.xx);
        float _657 = _115_oct_crop / max(1.0f - _115_oct_crop, 0.5f);
        float _659 = _648.x;
        float _662 = _648.y;
        if (max(_659 + (_657 * _662), _662 + (_657 * _659)) > 0.5f)
        {
            float2 param_14 = _320;
            outc = lerp(outc, backdrop(param_14), _115_oct_hide.xxx);
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
    float4 _696 = shade(param);
    float3 param_1 = clamp(_696.xyz, 0.0f.xxx, 1.0f.xxx);
    frag_color = float4(linear_to_srgb(param_1), _696.w);
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
