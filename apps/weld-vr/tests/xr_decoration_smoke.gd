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
	control.queue_free()
	await process_frame
	viewport.own_world_3d = true
	var camera := Camera3D.new()
	camera.projection = Camera3D.PROJECTION_ORTHOGONAL
	camera.size = 1.4
	camera.position.z = 1.0
	viewport.add_child(camera)
	var quad := QuadMesh.new()
	quad.size = Vector2(1.12, 0.72)
	var mesh := MeshInstance3D.new()
	mesh.mesh = quad
	shader = Shader.new()
	shader.code = FileAccess.get_file_as_string("res://rust/src/video/frame.gdshader")
	material = ShaderMaterial.new()
	material.shader = shader
	material.set_shader_parameter("window_size", Vector2(1.0, 0.6))
	mesh.material_override = material
	viewport.add_child(mesh)
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
	quad.size = Vector2(0.34, 0.05)
	camera.size = 0.4
	shader = Shader.new()
	shader.code = FileAccess.get_file_as_string("res://rust/src/video/xr/controls.gdshader")
	material = ShaderMaterial.new()
	material.shader = shader
	mesh.material_override = material
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
	viewport.queue_free()
	await process_frame
	print("WELD_XR_DECORATION_SMOKE_OK")
	quit(0)
