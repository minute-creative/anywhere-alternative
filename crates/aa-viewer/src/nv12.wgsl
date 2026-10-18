// NV12 (the video codec's own YUV layout) → RGB on the graphics card, so
// the processor never touches individual pixels. BT.709 "video range"
// (black = 16, white = 235), which is what every host sends.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var o: VsOut;
    o.pos = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = vec2<f32>((p[i].x + 1.0) * 0.5, 1.0 - (p[i].y + 1.0) * 0.5);
    return o;
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var uv_tex: texture_2d<f32>;

// The surface is sRGB: it re-encodes what we write. Our RGB is already
// gamma-encoded, so undo that first or the picture comes out washed out.
fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// Light sharpening, only when the picture is enlarged (a 1080p stream on a
// 2880-wide laptop screen is stretched 1.5x and looks soft, 0.5.8). How
// much: from how many video pixels one screen pixel covers. At 1:1 or when
// shrinking it is zero, so a picture shown at its own size is untouched.
// The detail comes from brightness only (an "unsharp mask" of the four
// neighbours), which sharpens edges without adding colour fringes.
fn sharpen_amount(uv: vec2<f32>) -> f32 {
    let dims = vec2<f32>(textureDimensions(y_tex));
    let texels_per_pixel = max(fwidth(uv.x) * dims.x, fwidth(uv.y) * dims.y);
    return clamp((1.0 - texels_per_pixel) * 0.9, 0.0, 0.45);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let amount = sharpen_amount(in.uv);
    let texel = 1.0 / vec2<f32>(textureDimensions(y_tex));
    let centre = textureSample(y_tex, smp, in.uv).r;
    let around = textureSample(y_tex, smp, in.uv + vec2<f32>(texel.x, 0.0)).r
        + textureSample(y_tex, smp, in.uv - vec2<f32>(texel.x, 0.0)).r
        + textureSample(y_tex, smp, in.uv + vec2<f32>(0.0, texel.y)).r
        + textureSample(y_tex, smp, in.uv - vec2<f32>(0.0, texel.y)).r;
    let luma = clamp(centre + amount * (centre - around * 0.25), 0.0, 1.0);
    let y = (luma - 16.0 / 255.0) * (255.0 / 219.0);
    let c = (textureSample(uv_tex, smp, in.uv).rg - vec2<f32>(128.0 / 255.0)) * (255.0 / 224.0);
    let rgb = vec3<f32>(
        y + 1.5748 * c.y,
        y - 0.1873 * c.x - 0.4681 * c.y,
        y + 1.8556 * c.x,
    );
    return vec4<f32>(to_linear(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
}
