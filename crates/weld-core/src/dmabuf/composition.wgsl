struct Geometry {
    destination: vec4<f32>,
    source: vec4<f32>,
    flags: vec4<f32>,
};
@group(0) @binding(0) var pixels: texture_2d<f32>;
@group(0) @binding(1) var filtering: sampler;
@group(1) @binding(0) var<uniform> geometry: Geometry;
struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@vertex fn vertex(@builtin(vertex_index) index: u32) -> Vertex {
    let corners = array<vec2<f32>, 6>(vec2(0., 0.), vec2(1., 0.), vec2(0., 1.), vec2(0., 1.), vec2(1., 0.), vec2(1., 1.));
    let corner = corners[index];
    let position = geometry.destination.xy + corner * geometry.destination.zw;
    var result: Vertex;
    result.position = vec4(position.x * 2. - 1., 1. - position.y * 2., 0., 1.);
    result.uv = geometry.source.xy + corner * geometry.source.zw;
    if geometry.flags.y > 0.5 { result.uv.y = 1. - result.uv.y; }
    return result;
}
@fragment fn fragment(input: Vertex) -> @location(0) vec4<f32> {
    var color = textureSample(pixels, filtering, input.uv);
    if geometry.flags.x > 0.5 { color.a = 1.; }
    return color;
}
