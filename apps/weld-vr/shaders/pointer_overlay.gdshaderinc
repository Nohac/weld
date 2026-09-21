// Reproduce the foreground portion of the world-space ray in a native canvas.
// Coordinates are in the window plane's meter-space, separately for each eye.
uniform bool pointer_visible = false;
uniform bool pointer_hit = false;
uniform vec2 pointer_canvas_size = vec2(1.0);
uniform vec3 pointer_eye;
uniform vec3 pointer_start;
uniform vec3 pointer_end;

vec4 with_pointer(vec4 canvas, vec2 uv) {
	if (!pointer_visible || canvas.a <= 0.0) { return canvas; }
	vec3 plane = vec3((uv.x - 0.5) * pointer_canvas_size.x, (0.5 - uv.y) * pointer_canvas_size.y, 0.0);
	vec3 view = plane - pointer_eye;
	vec3 segment = pointer_end - pointer_start;
	vec3 offset = pointer_eye - pointer_start;
	float a = dot(view, view);
	float b = dot(view, segment);
	float c = dot(segment, segment);
	if (a < 0.000001 || c < 0.000001) { return canvas; }
	float d = dot(view, offset);
	float e = dot(segment, offset);
	float denominator = a * c - b * b;
	float along = denominator > 0.000001 ? clamp((a * e - b * d) / denominator, 0.0, 1.0) : 1.0;
	vec3 closest = pointer_start + segment * along;
	float depth = dot(closest - pointer_eye, view) / a;
	float beam_distance = length(pointer_eye + view * depth - closest);
	float aa = max(fwidth(beam_distance), 0.0001);
	float beam = (1.0 - smoothstep(0.0015 - aa, 0.0015 + aa, beam_distance));
	beam *= step(0.0, depth) * step(depth, 1.0001);
	float tip_depth = dot(pointer_end - pointer_eye, view) / a;
	float tip_distance = length(pointer_eye + view * tip_depth - pointer_end);
	float tip_aa = max(fwidth(tip_distance), 0.0001);
	float tip = pointer_hit ? 1.0 - smoothstep(0.005 - tip_aa, 0.005 + tip_aa, tip_distance) : 0.0;
	tip *= step(0.0, tip_depth) * step(tip_depth, 1.0001);
	// Preserve canvas coverage: the normal 3D ray already occupies transparent
	// pixels below this layer. Do not turn shadows or margins into opaque quads.
	return vec4(mix(canvas.rgb, vec3(1.0, 0.16, 0.16) * canvas.a, max(beam, tip) * 0.8), canvas.a);
}
