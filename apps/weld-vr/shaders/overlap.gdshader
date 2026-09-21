// Shared by the Rust presenter and the on-device regression scene.
shader_type canvas_item;
render_mode unshaded, blend_disabled;
uniform sampler2D canvas_image : filter_nearest;
#include "res://shaders/pointer_overlay.gdshaderinc"
uniform float amount;
uniform int count;
// Bounded peer family, matching MAX_BACK_LAYERS. Explicit samplers keep the
// same shader valid on GLES drivers without dynamic sampler-array indexing.
uniform sampler2D peer_0 : filter_linear;
uniform sampler2D peer_1 : filter_linear;
uniform sampler2D peer_2 : filter_linear;
uniform sampler2D peer_3 : filter_linear;
uniform sampler2D peer_4 : filter_linear;
uniform sampler2D peer_5 : filter_linear;
uniform sampler2D peer_6 : filter_linear;
uniform sampler2D peer_7 : filter_linear;
uniform mat4 map_0;
uniform mat4 map_1;
uniform mat4 map_2;
uniform mat4 map_3;
uniform mat4 map_4;
uniform mat4 map_5;
uniform mat4 map_6;
uniform mat4 map_7;

// Viewport colors are premultiplied. Preserve alpha so non-overlap, rounded
// edges and shadows retain exactly their original coverage.
vec4 blend_orders(vec4 front, vec4 back, float weight) {
	return vec4(front.rgb * (1.0 - back.a * weight) + back.rgb * front.a * weight, front.a);
}

vec4 sample_peer(sampler2D source, mat4 mapping, vec2 uv) {
	vec4 projected = mapping * vec4(uv, 0.0, 1.0);
	if (projected.w <= 0.00001) { return vec4(0.0); }
	vec2 point = projected.xy / projected.w;
	if (any(lessThan(point, vec2(0.0))) || any(greaterThan(point, vec2(1.0)))) { return vec4(0.0); }
	return texture(source, point);
}

void fragment() {
	vec4 back = vec4(0.0);
	if (count > 0) { vec4 layer = sample_peer(peer_0, map_0, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 1) { vec4 layer = sample_peer(peer_1, map_1, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 2) { vec4 layer = sample_peer(peer_2, map_2, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 3) { vec4 layer = sample_peer(peer_3, map_3, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 4) { vec4 layer = sample_peer(peer_4, map_4, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 5) { vec4 layer = sample_peer(peer_5, map_5, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 6) { vec4 layer = sample_peer(peer_6, map_6, UV); back = layer + back * (1.0 - layer.a); }
	if (count > 7) { vec4 layer = sample_peer(peer_7, map_7, UV); back = layer + back * (1.0 - layer.a); }
	COLOR = with_pointer(blend_orders(texture(canvas_image, UV), back, amount), UV);
}
