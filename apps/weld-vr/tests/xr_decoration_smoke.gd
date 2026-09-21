extends SceneTree
## Real-renderer check of the shaders embedded by Rust, without a decoder or XR runtime.


func _initialize() -> void:
	call_deferred("_run")


func _run() -> void:
	var viewport := SubViewport.new()
	viewport.size = Vector2i(256, 256)
	viewport.transparent_bg = true
	viewport.render_target_update_mode = SubViewport.UPDATE_ALWAYS
	root.add_child(viewport)
	var shader := Shader.new()
	shader.code = "shader_type canvas_item;\nuniform sampler2D video;\n" + FileAccess.get_file_as_string("res://rust/src/video/stream.gdshaderinc")
	var material := ShaderMaterial.new()
	material.shader = shader
	var white := Image.create(1, 1, false, Image.FORMAT_RGBA8)
	white.fill(Color.WHITE)
	material.set_shader_parameter("video", ImageTexture.create_from_image(white))
	material.set_shader_parameter("has_frame", true)
	material.set_shader_parameter("corner_radius", 0.1)
	var control := ColorRect.new()
	control.size = Vector2(256, 256)
	control.material = material
	viewport.add_child(control)
	for index in range(4):
		await process_frame
		# Draw offscreen even when the desktop compositor occludes this probe.
		RenderingServer.force_draw(false)
	var image := viewport.get_texture().get_image()
	if image.get_pixel(128, 128).a < 0.95 or image.get_pixel(1, 1).a > 0.05:
		push_error("Video shader must preserve the center and clip rounded corners")
		quit(1)
		return
	# Eye selection happens inside the existing content crop, excluding coded
	# padding. Both materials sample the same texture without another decoder.
	var stereo_image := Image.create(8, 2, false, Image.FORMAT_RGBA8)
	stereo_image.fill(Color.BLUE)
	for y in range(2):
		for x in range(2, 4):
			stereo_image.set_pixel(x, y, Color.RED)
		for x in range(4, 6):
			stereo_image.set_pixel(x, y, Color.GREEN)
	material.set_shader_parameter("video", ImageTexture.create_from_image(stereo_image))
	material.set_shader_parameter("crop", Vector4(0.25, 0.0, 0.75, 1.0))
	for right in [false, true]:
		material.set_shader_parameter("eye_view", Vector4(0.5, 0, 1, 1) if right else Vector4(0, 0, 0.5, 1))
		for index in range(4):
			await process_frame
			RenderingServer.force_draw(false)
		var eye_pixel := viewport.get_texture().get_image().get_pixel(128, 128)
		if (right and (eye_pixel.g < 0.9 or eye_pixel.r > 0.1)) or (not right and (eye_pixel.r < 0.9 or eye_pixel.g > 0.1)):
			push_error("Packed stereo eye selection or content crop is incorrect")
			quit(1)
			return
		if viewport.get_texture().get_image().get_pixel(1, 1).a > 0.05:
			push_error("Both stereo eyes must share the window corner clipping")
			quit(1)
			return
	material.set_shader_parameter("content_fit", Vector2(0.5, 1.0))
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	image = viewport.get_texture().get_image()
	if image.get_pixel(128, 128).g < 0.9 or image.get_pixel(32, 128).g > 0.1:
		push_error("Resize preview must letterbox the image while preserving eye selection")
		quit(1)
		return
	material.set_shader_parameter("content_fit", Vector2.ONE)
	# An inset child has square local corners inside the window. A child at
	# the owner's top-left clips only that outer corner, not its own right edge.
	for region in [Vector4(0.25, 0.25, 0.5, 0.5), Vector4(0, 0, 0.5, 0.5)]:
		material.set_shader_parameter("window_region", region)
		for index in range(4):
			await process_frame
			RenderingServer.force_draw(false)
		image = viewport.get_texture().get_image()
		if image.get_pixel(254, 254).a < 0.95 \
			or (region.x > 0.0 and image.get_pixel(1, 1).a < 0.95) \
			or (region.x == 0.0 and image.get_pixel(1, 1).a > 0.05):
			push_error("Subsurface clipping must use the shared window outline")
			quit(1)
			return
	control.queue_free()
	await process_frame
	# Decorations now share the native canvas with content, not scene depth.
	var overlay := ColorRect.new()
	overlay.size = Vector2(1.12, 0.72) * (256.0 / 1.4)
	overlay.position = (Vector2(256, 256) - overlay.size) * 0.5
	shader = Shader.new()
	shader.code = FileAccess.get_file_as_string("res://rust/src/video/frame.gdshader")
	material = ShaderMaterial.new()
	material.shader = shader
	material.set_shader_parameter("window_size", Vector2(1.0, 0.6))
	overlay.material = material
	viewport.add_child(overlay)
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	image = viewport.get_texture().get_image()
	var border_alpha := 0.0
	for x in range(217, 222):
		border_alpha = maxf(border_alpha, image.get_pixel(x, 128).a)
	var shadow_alpha := image.get_pixel(223, 128).a
	if image.get_pixel(128, 128).a > 0.01 or image.get_pixel(250, 128).a > 0.01 \
		or border_alpha < 0.3 or shadow_alpha < 0.01 or shadow_alpha >= border_alpha:
		push_error("Outline/shadow alpha is incorrect: border=%s shadow=%s" % [border_alpha, shadow_alpha])
		quit(1)
		return
	var inactive_border := image.get_pixel(219, 128)
	material.set_shader_parameter("focused", true)
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	image = viewport.get_texture().get_image()
	var active_border := image.get_pixel(219, 128)
	if active_border.b <= inactive_border.b or active_border.r >= inactive_border.r \
		or image.get_pixel(128, 128).a > 0.01:
		push_error("Focused border must turn blue without covering window content")
		quit(1)
		return
	material.set_shader_parameter("focused", false)
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	if not viewport.get_texture().get_image().get_pixel(219, 128).is_equal_approx(inactive_border):
		push_error("Losing focus must restore the inactive border")
		quit(1)
		return
	var before_handles := viewport.get_texture().get_image()
	material.set_shader_parameter("resize_opacity", 1.0)
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	image = viewport.get_texture().get_image()
	var handle_gain := 0.0
	for y in range(70, 85):
		for x in range(33, 50):
			handle_gain = maxf(handle_gain, image.get_pixel(x, y).a - before_handles.get_pixel(x, y).a)
	if handle_gain < 0.15 or image.get_pixel(128, 128).a > 0.01:
		push_error("Resize corners must be visible without covering the window center")
		quit(1)
		return
	var outside_gain := 0.0
	for y in range(66, 83):
		for x in range(30, 50):
			if x < 35 or y < 72:
				outside_gain = maxf(outside_gain, image.get_pixel(x, y).a - before_handles.get_pixel(x, y).a)
	if absf(image.get_pixel(37, 80).a - before_handles.get_pixel(37, 80).a) > 0.05 or outside_gain < 0.15:
		push_error("Resize handles must float outside the unchanged window border")
		quit(1)
		return
	overlay.size = Vector2(0.46, 0.05) * (256.0 / 0.5)
	overlay.position = (Vector2(256, 256) - overlay.size) * 0.5
	shader = Shader.new()
	shader.code = FileAccess.get_file_as_string("res://rust/src/video/xr/controls.gdshader")
	material = ShaderMaterial.new()
	material.shader = shader
	overlay.material = material
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	image = viewport.get_texture().get_image()
	if image.get_pixel(128, 128).a < 0.95 or image.get_pixel(10, 10).a > 0.01 \
		or image.get_pixel(123, 128).r <= image.get_pixel(128, 119).r \
		or image.get_pixel(72, 128).r < image.get_pixel(128, 119).r + 0.2 \
		or image.get_pixel(174, 128).r < image.get_pixel(128, 119).r + 0.2:
		push_error("Window controls pixels: center=%s handle=%s background=%s minus=%s plus=%s" % [image.get_pixel(128, 128), image.get_pixel(123, 128), image.get_pixel(128, 119), image.get_pixel(72, 128), image.get_pixel(174, 128)])
		quit(1)
		return
	material.set_shader_parameter("hovered", 3)
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	var gamepad_region := viewport.get_texture().get_image().get_pixel(204, 124)
	if gamepad_region.b <= gamepad_region.r:
		push_error("Gamepad hover must highlight the right-hand controls region")
		quit(1)
		return
	material.set_shader_parameter("opacity", 0.25)
	for index in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	if absf(viewport.get_texture().get_image().get_pixel(128, 128).a - 0.25) > 0.01:
		push_error("Window controls must fade their background and icons together")
		quit(1)
		return
	overlay.queue_free()
	await process_frame
	if not await _check_overlap(viewport):
		quit(1)
		return
	if not await _check_pointer(viewport):
		quit(1)
		return
	viewport.queue_free()
	await process_frame
	print("WELD_XR_DECORATION_SMOKE_OK")
	quit(0)


func _check_pointer(viewport: SubViewport) -> bool:
	var shader := load("res://shaders/native_canvas.gdshader") as Shader
	var material := ShaderMaterial.new()
	material.shader = shader
	var pixels := Image.create(1, 1, false, Image.FORMAT_RGBA8)
	pixels.fill(Color(0, 0, 0.4, 0.4))
	material.set_shader_parameter("canvas_image", ImageTexture.create_from_image(pixels))
	material.set_shader_parameter("pointer_visible", true)
	material.set_shader_parameter("pointer_hit", true)
	material.set_shader_parameter("pointer_eye", Vector3(0, 0, 1))
	material.set_shader_parameter("pointer_start", Vector3(-0.15, -0.15, 0.5))
	material.set_shader_parameter("pointer_end", Vector3.ZERO)
	var rect := ColorRect.new()
	rect.size = Vector2(256, 256)
	rect.material = material
	viewport.add_child(rect)
	for frame in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	var image := viewport.get_texture().get_image()
	if image.get_pixel(128, 128).r < 0.1 or image.get_pixel(90, 166).r < 0.05 \
		or absf(image.get_pixel(128, 128).a - 0.4) > 0.01:
		push_error("Native pointer must retain beam, hit marker and window alpha")
		return false
	material.set_shader_parameter("pointer_start", Vector3(0, 0, -0.1))
	material.set_shader_parameter("pointer_end", Vector3(0, 0, -0.5))
	for frame in range(4):
		await process_frame
		RenderingServer.force_draw(false)
	if viewport.get_texture().get_image().get_pixel(128, 128).r > 0.01:
		push_error("Native pointer must not reveal a ray behind the window")
		return false
	material.set_shader_parameter("pointer_start", Vector3(0, 0, 0.75))
	material.set_shader_parameter("pointer_end", Vector3(0, 0, 0.5))
	var centers: Array[float] = []
	for eye in [-0.03, 0.03]:
		material.set_shader_parameter("pointer_eye", Vector3(eye, 0, 1))
		for frame in range(4):
			await process_frame
			RenderingServer.force_draw(false)
		image = viewport.get_texture().get_image()
		var weight := 0.0
		var total := 0.0
		for x in range(256):
			var red := image.get_pixel(x, 128).r
			total += x * red
			weight += red
		if weight <= 0.0:
			push_error("Eye-specific native pointer is missing")
			return false
		centers.append(total / weight)
	if centers[0] - centers[1] < 8.0:
		push_error("Native pointer must use each eye's own projection")
		return false
	rect.queue_free()
	await process_frame
	return true


func _check_overlap(viewport: SubViewport) -> bool:
	# The native output shader reads ordinary source canvases, never its own
	# render target or another native swapchain. Include layered local content.
	var output := SubViewport.new()
	output.size = Vector2i(256, 256)
	output.transparent_bg = true
	output.render_target_update_mode = SubViewport.UPDATE_ALWAYS
	root.add_child(output)
	var back := SubViewport.new()
	back.size = Vector2i(256, 256)
	back.transparent_bg = true
	back.render_target_update_mode = SubViewport.UPDATE_ALWAYS
	root.add_child(back)
	# Mirror the production render dependency: children draw before parents.
	RenderingServer.viewport_set_parent_viewport(back.get_viewport_rid(), output.get_viewport_rid())
	RenderingServer.viewport_set_parent_viewport(viewport.get_viewport_rid(), output.get_viewport_rid())
	var peer := ColorRect.new()
	peer.size = Vector2(128, 256)
	var peer_shader := Shader.new()
	peer_shader.code = "shader_type canvas_item; uniform vec4 tint = vec4(1, 0, 0, 1); void fragment() { COLOR = tint; }"
	var peer_material := ShaderMaterial.new()
	peer_material.shader = peer_shader
	peer.material = peer_material
	back.add_child(peer)
	var base := ColorRect.new()
	base.size = Vector2(224, 256)
	base.color = Color.GREEN
	viewport.add_child(base)
	var child := ColorRect.new()
	child.size = Vector2(128, 128)
	child.color = Color.BLUE
	viewport.add_child(child)
	var shadow := ColorRect.new()
	shadow.position = Vector2(224, 0)
	shadow.size = Vector2(16, 256)
	shadow.color = Color(0, 1, 0, 0.25)
	viewport.add_child(shadow)
	var shader := Shader.new()
	shader.code = FileAccess.get_file_as_string("res://shaders/overlap.gdshader")
	var material := ShaderMaterial.new()
	material.shader = shader
	material.set_shader_parameter("canvas_image", viewport.get_texture())
	material.set_shader_parameter("peer_0", back.get_texture())
	material.set_shader_parameter("map_0", Projection.IDENTITY)
	material.set_shader_parameter("count", 1)
	var pass_rect := ColorRect.new()
	pass_rect.size = Vector2(256, 256)
	pass_rect.z_index = 100
	pass_rect.material = material
	output.add_child(pass_rect)
	for amount in [0.0, 0.5, 1.0, 0.25]:
		material.set_shader_parameter("amount", amount)
		for index in range(4):
			await process_frame
			RenderingServer.force_draw(false)
		var image := output.get_texture().get_image()
		var expected_child := Color(amount, 0, 1 - amount, 1)
		var expected_base := Color(amount, 1 - amount, 0, 1)
		if not _near_color(image.get_pixel(64, 64), expected_child) \
			or not _near_color(image.get_pixel(64, 180), expected_base) \
			or not _near_color(image.get_pixel(190, 64), Color.GREEN) \
			or absf(image.get_pixel(230, 64).a - 0.25) > 0.01 \
			or image.get_pixel(250, 64).a > 0.01:
			push_error("Overlap blend must preserve non-overlap, layered content and alpha: amount=%s child=%s base=%s outside=%s" % [amount, image.get_pixel(64, 64), image.get_pixel(64, 180), image.get_pixel(190, 64)])
			return false
	# Dependency ordering must use this draw's peer pixels, not one-frame-old
	# colors. No settling frames here: peer and consumer update together.
	material.set_shader_parameter("amount", 1.0)
	for color in [Color.BLUE, Color.GREEN, Color.RED]:
		# A uniform update goes straight to the renderer, without ColorRect's
		# deferred queue_redraw. Exactly one draw must carry the new pixels.
		peer_material.set_shader_parameter("tint", color)
		RenderingServer.force_draw(false)
		if not _near_color(output.get_texture().get_image().get_pixel(64, 64), color):
			push_error("Overlap sampling used stale peer canvas pixels: expected=%s peer=%s result=%s" % [color, back.get_texture().get_image().get_pixel(64, 64), output.get_texture().get_image().get_pixel(64, 64)])
			return false
		await process_frame
	RenderingServer.viewport_set_parent_viewport(viewport.get_viewport_rid(), root.get_viewport_rid())
	pass_rect.queue_free()
	back.queue_free()
	output.queue_free()
	return true


func _near_color(actual: Color, expected: Color) -> bool:
	return absf(actual.r - expected.r) < 0.015 and absf(actual.g - expected.g) < 0.015 \
		and absf(actual.b - expected.b) < 0.015 and absf(actual.a - expected.a) < 0.015
