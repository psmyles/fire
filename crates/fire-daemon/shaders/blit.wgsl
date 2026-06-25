// Phase-1 passthrough blit: a full-screen triangle samples the image texture and
// stretches it across the window. Aspect-correct fit/pan/zoom and the channel/alpha/
// gamma/exposure/tonemap shader are Phase 3; here the window is sized to the image's
// aspect so the stretch reads correctly.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Oversized triangle covering the [-1,1] clip square; the excess is clipped.
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let xy = corners[vid];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    // Map clip space to texture UV (origin top-left → flip Y).
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

@group(0) @binding(0) var image_tex: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(image_tex, image_sampler, in.uv);
}
