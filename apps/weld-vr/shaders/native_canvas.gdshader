// Shared by the Rust presenter and the on-device regression scene.
shader_type canvas_item;
render_mode unshaded, blend_disabled;
uniform sampler2D canvas_image : filter_nearest;

void fragment() {
	// The source is already premultiplied. Copy exactly, without blending its
	// alpha a second time or reading back from the native OpenXR swapchain.
	COLOR = texture(canvas_image, UV);
}
