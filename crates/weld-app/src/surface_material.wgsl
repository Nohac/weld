#import bevy_ui::ui_vertex_output::{UiVertexOutput}

struct SurfaceMaterialParameters {
    source_rect: vec4<f32>,
    buffer_size: vec2<f32>,
    flags: vec2<u32>,
}

@group(1) @binding(0)
var surface_texture: texture_2d<f32>;
@group(1) @binding(1)
var<uniform> parameters: SurfaceMaterialParameters;

const LINEAR_STRAIGHT: u32 = 0u;
const ENCODED_PREMULTIPLIED: u32 = 1u;
const ENCODED_OPAQUE: u32 = 2u;
const UNBOUND: u32 = 3u;
const ALIGNMENT_EPSILON: f32 = 0.001;

fn srgb_to_linear_channel(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        return encoded / 12.92;
    }
    return pow((encoded + 0.055) / 1.055, 2.4);
}

fn srgb_to_linear(encoded: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        srgb_to_linear_channel(encoded.r),
        srgb_to_linear_channel(encoded.g),
        srgb_to_linear_channel(encoded.b),
    );
}

fn normalized_texel(coordinate: vec2<i32>, dimensions: vec2<i32>) -> vec4<f32> {
    let clamped = clamp(coordinate, vec2<i32>(0), dimensions - vec2<i32>(1));
    let encoded = textureLoad(surface_texture, clamped, 0);
    if parameters.flags.x == LINEAR_STRAIGHT {
        return encoded;
    }
    if parameters.flags.x == ENCODED_OPAQUE {
        return vec4<f32>(srgb_to_linear(encoded.rgb), 1.0);
    }
    if encoded.a <= 0.0 {
        return vec4<f32>(0.0);
    }
    let straight_encoded = min(encoded.rgb / encoded.a, vec3<f32>(1.0));
    return vec4<f32>(srgb_to_linear(straight_encoded), encoded.a);
}

fn source_pixel(uv: vec2<f32>) -> vec2<f32> {
    var pixel = parameters.source_rect.xy + uv * parameters.source_rect.zw;
    if parameters.flags.y != 0u {
        // Y_INVERT orients the complete buffer before the logical viewport
        // source rectangle selects from it.
        pixel.y = parameters.buffer_size.y - pixel.y;
    }
    return pixel;
}

fn is_pixel_aligned_one_to_one(pixel: vec2<f32>) -> bool {
    let dx = dpdx(pixel);
    let dy = dpdy(pixel);
    let expected_dy = select(vec2<f32>(0.0, 1.0), vec2<f32>(0.0, -1.0), parameters.flags.y != 0u);
    let unit_scale = all(abs(dx - vec2<f32>(1.0, 0.0)) < vec2<f32>(ALIGNMENT_EPSILON))
        && all(abs(dy - expected_dy) < vec2<f32>(ALIGNMENT_EPSILON));
    let center = pixel - vec2<f32>(0.5);
    let center_aligned = all(abs(center - round(center)) < vec2<f32>(ALIGNMENT_EPSILON));
    return unit_scale && center_aligned;
}

fn sample_surface(pixel: vec2<f32>) -> vec4<f32> {
    let dimensions = vec2<i32>(textureDimensions(surface_texture));
    let centered = pixel - vec2<f32>(0.5);
    if is_pixel_aligned_one_to_one(pixel) {
        return normalized_texel(vec2<i32>(round(centered)), dimensions);
    }

    let lower = floor(centered);
    let fraction = fract(centered);
    let base = vec2<i32>(lower);
    // Match ClampToEdge on the complete image. Deliberately do not clamp to
    // the viewport crop: Bevy's previous ImageNode could filter across it.
    let top_left = normalized_texel(base, dimensions);
    let top_right = normalized_texel(base + vec2<i32>(1, 0), dimensions);
    let bottom_left = normalized_texel(base + vec2<i32>(0, 1), dimensions);
    let bottom_right = normalized_texel(base + vec2<i32>(1, 1), dimensions);
    return mix(
        mix(top_left, top_right, fraction.x),
        mix(bottom_left, bottom_right, fraction.x),
        fraction.y,
    );
}

fn rounded_box_distance(point: vec2<f32>, size: vec2<f32>, radii: vec4<f32>) -> f32 {
    let pair = select(radii.xy, radii.wz, point.y > 0.0);
    let radius = select(pair.x, pair.y, point.x > 0.0);
    let offset = abs(point) - 0.5 * size + radius;
    return length(max(offset, vec2<f32>(0.0)))
        + min(max(offset.x, offset.y), 0.0)
        - radius;
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    if parameters.flags.x == UNBOUND {
        return vec4<f32>(0.0);
    }
    var color = sample_surface(source_pixel(in.uv));
    let point = (in.uv - vec2<f32>(0.5)) * in.size;
    let edge_alpha = saturate(0.5 - rounded_box_distance(point, in.size, in.border_radius));
    color.a *= edge_alpha;
    return color;
}
