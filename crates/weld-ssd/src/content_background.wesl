import bevy_ui_render::{ui_vertex_output::UiVertexOutput, ui::sd_rounded_box};

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> covered: vec4<f32>;

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    if all(in.uv >= covered.xy) && all(in.uv < covered.zw) {
        discard;
    }
    let point = (in.uv - vec2<f32>(0.5)) * in.size;
    let distance = sd_rounded_box(point, in.size, in.border_radius_x, in.border_radius_y);
    let alpha = saturate(0.5 - distance / max(fwidth(distance), 0.0001));
    return vec4<f32>(color.rgb, color.a * alpha);
}
