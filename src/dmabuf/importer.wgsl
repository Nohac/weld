struct ImportOptions {
    y_inverted: u32,
    force_opaque: u32,
    _padding: vec2<u32>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<uniform> options: ImportOptions;

@vertex
fn vertex(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let dimensions = textureDimensions(source);
    let x = min(u32(position.x), dimensions.x - 1u);
    let output_y = min(u32(position.y), dimensions.y - 1u);
    let source_y = select(output_y, dimensions.y - output_y - 1u, options.y_inverted != 0u);
    var encoded = textureLoad(source, vec2<i32>(i32(x), i32(source_y)), 0);
    if options.force_opaque != 0u {
        encoded = vec4<f32>(encoded.rgb, 1.0);
    } else if encoded.a > 0.0 {
        encoded = vec4<f32>(min(encoded.rgb / encoded.a, vec3<f32>(1.0)), encoded.a);
    } else {
        encoded = vec4<f32>(0.0);
    }
    return encoded;
}
