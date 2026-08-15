struct VertexOutput {
    @builtin(position) position: vec4<f32>,
}

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return VertexOutput(vec4<f32>(positions[index], 0.0, 1.0));
}

struct CursorOverlay {
    origin: vec2<f32>,
    extent: vec2<f32>,
    source_origin: vec2<f32>,
    source_extent: vec2<f32>,
    texture_size: vec2<f32>,
    visible: f32,
    _padding: f32,
}

@group(0) @binding(0) var<uniform> cursor: CursorOverlay;
@group(0) @binding(1) var cursor_texture: texture_2d<f32>;

const ALIGNMENT_EPSILON: f32 = 0.001;

fn premultiplied_cursor_texel(coordinate: vec2<i32>) -> vec4<f32> {
    let dimensions = vec2<i32>(cursor.texture_size);
    let clamped = clamp(coordinate, vec2<i32>(0), dimensions - vec2<i32>(1));
    let straight = textureLoad(cursor_texture, clamped, 0);
    return vec4<f32>(straight.rgb * straight.a, straight.a);
}

fn is_pixel_aligned_one_to_one(pixel: vec2<f32>) -> bool {
    let dx = dpdx(pixel);
    let dy = dpdy(pixel);
    let unit_scale = all(abs(dx - vec2<f32>(1.0, 0.0)) < vec2<f32>(ALIGNMENT_EPSILON))
        && all(abs(dy - vec2<f32>(0.0, 1.0)) < vec2<f32>(ALIGNMENT_EPSILON));
    let center = pixel - vec2<f32>(0.5);
    let center_aligned = all(abs(center - round(center)) < vec2<f32>(ALIGNMENT_EPSILON));
    return unit_scale && center_aligned;
}

fn sample_cursor(pixel: vec2<f32>) -> vec4<f32> {
    let centered = pixel - vec2<f32>(0.5);
    if is_pixel_aligned_one_to_one(pixel) {
        return premultiplied_cursor_texel(vec2<i32>(round(centered)));
    }

    let lower = floor(centered);
    let fraction = fract(centered);
    let base = vec2<i32>(lower);
    let top_left = premultiplied_cursor_texel(base);
    let top_right = premultiplied_cursor_texel(base + vec2<i32>(1, 0));
    let bottom_left = premultiplied_cursor_texel(base + vec2<i32>(0, 1));
    let bottom_right = premultiplied_cursor_texel(base + vec2<i32>(1, 1));
    return mix(
        mix(top_left, top_right, fraction.x),
        mix(bottom_left, bottom_right, fraction.x),
        fraction.y,
    );
}

@fragment
fn fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    if cursor.visible == 0.0 || any(cursor.extent <= vec2<f32>(0.0)) {
        discard;
    }
    let relative = input.position.xy - cursor.origin;
    if any(relative < vec2<f32>(0.0)) || any(relative >= cursor.extent) {
        discard;
    }
    let uv = relative / cursor.extent;
    return sample_cursor(cursor.source_origin + uv * cursor.source_extent);
}
