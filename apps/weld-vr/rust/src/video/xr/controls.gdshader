shader_type spatial;
render_mode unshaded, cull_disabled, depth_draw_never, fog_disabled;
uniform int hovered = 0;
uniform float opacity : hint_range(0.0, 1.0) = 1.0;

float pill(vec2 point, vec2 half_size, float radius) {
	vec2 q = abs(point) - half_size + radius;
	return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}
void fragment() {
	vec2 p = (UV - 0.5) * vec2(0.34, 0.05);
	float d = pill(p, vec2(0.17, 0.025), 0.0225);
	float aa = max(fwidth(d), 0.0001);
	vec3 color = mix(vec3(0.035, 0.043, 0.055), vec3(0.065, 0.075, 0.09), UV.y);
	bool close_area = p.x < -0.06;
	if ((hovered == 1 && !close_area) || (hovered == 2 && close_area)) {
		color = close_area ? vec3(0.3, 0.045, 0.06) : vec3(0.07, 0.19, 0.32);
	}
	float handle = pill(p - vec2(0.045, 0.0), vec2(0.052, 0.003), 0.003);
	vec2 cross_point = p + vec2(0.115, 0.0);
	vec2 rotated = vec2(cross_point.x + cross_point.y, cross_point.y - cross_point.x) * 0.707107;
	float cross_d = min(pill(rotated, vec2(0.012, 0.0015), 0.001), pill(rotated, vec2(0.0015, 0.012), 0.001));
	float icon = 1.0 - smoothstep(-aa, aa, min(handle, cross_d));
	ALBEDO = mix(color, vec3(0.85, 0.9, 0.98), icon);
	ALPHA = opacity * (1.0 - smoothstep(-aa, aa, d));
}
