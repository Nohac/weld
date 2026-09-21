shader_type canvas_item;
uniform int hovered = 0;
uniform float opacity : hint_range(0.0, 1.0) = 1.0;

float pill(vec2 point, vec2 half_size, float radius) {
	vec2 q = abs(point) - half_size + radius;
	return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}
void fragment() {
	vec2 p = (UV - 0.5) * vec2(0.46, 0.05);
	float d = pill(p, vec2(0.23, 0.025), 0.0225);
	float aa = max(fwidth(d), 0.0001);
	vec3 color = mix(vec3(0.035, 0.043, 0.055), vec3(0.065, 0.075, 0.09), UV.y);
	bool close_area = p.x < -0.15;
	int area = close_area ? 2 : (p.x < -0.075 ? 4 : (p.x < 0.055 ? 1 : (p.x < 0.13 ? 5 : 3)));
	if (hovered == area) {
		color = close_area ? vec3(0.3, 0.045, 0.06) : vec3(0.07, 0.19, 0.32);
	}
	float handle = pill(p + vec2(0.01, 0.0), vec2(0.035, 0.003), 0.003);
	vec2 g = p - vec2(0.18, 0.0);
	float body = abs(pill(g, vec2(0.022, 0.011), 0.006)) - 0.0015;
	float pad = min(pill(g + vec2(0.01, 0.0), vec2(0.006, 0.0015), 0.001), pill(g + vec2(0.01, 0.0), vec2(0.0015, 0.006), 0.001));
	float buttons = min(length(g - vec2(0.011, 0.004)), length(g - vec2(0.016, -0.004))) - 0.002;
	float gamepad = min(body, min(pad, buttons));
	vec2 cross_point = p + vec2(0.19, 0.0);
	vec2 rotated = vec2(cross_point.x + cross_point.y, cross_point.y - cross_point.x) * 0.707107;
	float cross_d = min(pill(rotated, vec2(0.012, 0.0015), 0.001), pill(rotated, vec2(0.0015, 0.012), 0.001));
	float minus = pill(p + vec2(0.11, 0.0), vec2(0.010, 0.0015), 0.001);
	vec2 plus_point = p - vec2(0.09, 0.0);
	float plus = min(pill(plus_point, vec2(0.010, 0.0015), 0.001), pill(plus_point, vec2(0.0015, 0.010), 0.001));
	float icon = 1.0 - smoothstep(-aa, aa, min(min(minus, plus), min(gamepad, min(handle, cross_d))));
	COLOR = vec4(mix(color, vec3(0.85, 0.9, 0.98), icon), opacity * (1.0 - smoothstep(-aa, aa, d)));
}
