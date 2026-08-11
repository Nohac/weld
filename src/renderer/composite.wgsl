struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[index];
    return VertexOutput(
        vec4<f32>(position, 0.0, 1.0),
        position * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5),
    );
}

@group(0) @binding(0) var layer_texture: texture_2d<f32>;
@group(0) @binding(1) var layer_sampler: sampler;

struct CursorOverlay {
    center: vec2<f32>,
    radius: f32,
    visible: f32,
}

@group(0) @binding(2) var<uniform> cursor: CursorOverlay;

@fragment
fn fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    let composition = textureSample(layer_texture, layer_sampler, input.uv);
    let distance_from_cursor = distance(input.position.xy, cursor.center);
    let coverage = cursor.visible * clamp(cursor.radius + 0.5 - distance_from_cursor, 0.0, 1.0);
    return mix(composition, vec4<f32>(1.0), coverage);
}
