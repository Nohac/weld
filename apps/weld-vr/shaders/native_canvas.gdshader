// Shared by the Rust presenter and the on-device regression scene.
shader_type canvas_item;
render_mode unshaded, blend_disabled;
uniform sampler2D canvas_image : filter_nearest;
#include "res://shaders/pointer_overlay.gdshaderinc"

void fragment() {
	// Preserve the premultiplied source/coverage, adding only the foreground
	// pointer. Never sample or read back the native OpenXR swapchain itself.
	COLOR = with_pointer(texture(canvas_image, UV), UV);
}
