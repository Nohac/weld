shader_type spatial;
render_mode unshaded, cull_disabled, depth_draw_never, fog_disabled;

uniform vec2 window_size = vec2(1.0);
uniform float corner_radius = 0.012;
uniform float margin = 0.06;
uniform vec4 border_color : source_color = vec4(0.48, 0.52, 0.60, 0.72);
uniform vec4 focused_border_color : source_color = vec4(0.2, 0.65, 1.0, 0.95);
uniform bool focused = false;

float rounded_distance(vec2 point) {
	vec2 q = abs(point) - (window_size * 0.5 - corner_radius);
	return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - corner_radius;
}

void fragment() {
	vec2 point = (UV - 0.5) * (window_size + vec2(2.0 * margin));
	float distance = rounded_distance(point);
	float antialias = max(fwidth(distance), 0.00015);
	vec4 color = focused ? focused_border_color : border_color;
	float thickness = focused ? 0.0018 : 0.001;
	float border = (1.0 - smoothstep(thickness - antialias, thickness + antialias, abs(distance))) * color.a;
	float sigma = min(0.014, min(window_size.x, window_size.y) * 0.1);
	float shadow_distance = max(rounded_distance(point - vec2(0.0, sigma * 0.5)), 0.0);
	float shadow = 0.5 * exp(-0.5 * pow(shadow_distance / sigma, 2.0));
	shadow *= smoothstep(-antialias, antialias, distance);
	float alpha = border + shadow * (1.0 - border);
	ALBEDO = color.rgb * border / max(alpha, 0.00001);
	ALPHA = alpha;
}
