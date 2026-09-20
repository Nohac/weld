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
	overlay.size = Vector2(0.34, 0.05) * (256.0 / 0.4)
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
		or image.get_pixel(156, 128).r <= image.get_pixel(128, 116).r \
		or image.get_pixel(54, 128).r <= image.get_pixel(202, 128).r:
		push_error("Window controls must render a rounded strip with a visible drag handle")
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
	viewport.queue_free()
	await process_frame
	print("WELD_XR_DECORATION_SMOKE_OK")
	quit(0)


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
