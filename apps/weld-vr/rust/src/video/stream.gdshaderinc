uniform vec4 crop = vec4(0.0, 0.0, 1.0, 1.0);
uniform bool has_frame = false;
uniform vec2 window_size = vec2(1.0);
uniform float corner_radius = 0.0;
// Content subsurfaces clip to their owning window, not independent rectangles.
uniform vec4 window_region = vec4(0.0, 0.0, 1.0, 1.0);
// Eye selection is inside the protocol content crop, not the coded padding.
uniform vec4 eye_view = vec4(0.0, 0.0, 1.0, 1.0);
// Resize preview fits the old image without stretching either eye.
uniform vec2 content_fit = vec2(1.0);

void fragment() {
	float coverage = 1.0;
	if (corner_radius > 0.0) {
		vec2 window_uv = window_region.xy + UV * window_region.zw;
		vec2 q = abs((window_uv - 0.5) * window_size) - (window_size * 0.5 - corner_radius);
		float distance = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - corner_radius;
		float antialias = max(fwidth(distance), 0.00001);
		coverage = 1.0 - smoothstep(-antialias, antialias, distance);
	}
	vec2 content_uv = (UV - 0.5) / content_fit + 0.5;
	bool in_content = all(greaterThanEqual(content_uv, vec2(0.0))) && all(lessThanEqual(content_uv, vec2(1.0)));
	vec2 eye_uv = mix(eye_view.xy, eye_view.zw, clamp(content_uv, vec2(0.0), vec2(1.0)));
	COLOR = has_frame ? vec4(in_content ? texture(video, mix(crop.xy, crop.zw, eye_uv)).rgb : vec3(0.025), coverage) : vec4(0.0);
}
