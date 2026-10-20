// Draws the decoded picture as one rectangle, letterboxed by the caller.
#include <metal_stdlib>
using namespace metal;

struct VOut {
    float4 pos [[position]];
    float2 uv;
};

// `rect`: left, top, right, bottom of the picture in clip space (-1…1).
// `win`: the part of the picture shown (x, y, width, height in 0…1); the
// whole picture is (0, 0, 1, 1), zoom shows a smaller window.
vertex VOut stream_vertex(uint vid [[vertex_id]], constant float4 &rect [[buffer(0)]], constant float4 &win [[buffer(1)]]) {
    float2 corners[4] = { float2(rect.x, rect.w), float2(rect.z, rect.w), float2(rect.x, rect.y), float2(rect.z, rect.y) };
    float2 uvs[4] = { float2(0, 1), float2(1, 1), float2(0, 0), float2(1, 0) };
    VOut o;
    o.pos = float4(corners[vid], 0, 1);
    o.uv = win.xy + uvs[vid] * win.zw;
    return o;
}

fragment float4 stream_fragment(VOut in [[stage_in]], texture2d<float> tex [[texture(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    return float4(tex.sample(s, in.uv).rgb, 1.0);
}
