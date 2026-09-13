extends SceneTree
## Run with a real EGL display, never headless. Saves one diagnostic screenshot.

var draw_count := 0


func _before_draw() -> void:
	draw_count += 1


func _initialize() -> void:
	RenderingServer.frame_pre_draw.connect(_before_draw)
	call_deferred("_run")


func _run() -> void:
	var scene: PackedScene = load("res://scenes/main.tscn")
	var main := scene.instantiate()
	root.add_child(main)
	await process_frame
	var panel := main.get_node("VideoPanel")
	# process_frame advances even when the display is not drawing. Do not start
	# decoding until this real-window test has observed a render callback.
	var waiting := Time.get_ticks_msec()
	while draw_count == 0 and Time.get_ticks_msec() - waiting < 5000:
		await create_timer(0.05).timeout
	print("WELD_VIDEO_DRAW_START count=", draw_count,
		" wait_ms=", Time.get_ticks_msec() - waiting)
	if draw_count == 0:
		push_error("No frame_pre_draw callbacks; check window visibility/render-loop before testing video")
		quit(1)
		return
	for attempt in range(3):
		var started := Time.get_ticks_msec()
		var first_draw := draw_count
		var expected_frames := 1 if attempt == 0 else 120
		if attempt == 0:
			# One AU, no later input or EOS. Desktop validates fixture plumbing;
			# Android's asynchronous MediaCodec contract still needs a device test.
			panel._start(false, true)
		else:
			# A naturally completed native session must accept a fresh target
			# without relying on the UI's usual explicit stop-before-play.
			var texture := ExternalTexture.new()
			texture.size = Vector2(320, 180)
			var material := panel.video_material.duplicate() as ShaderMaterial
			material.set_shader_parameter("video", texture)
			material.set_shader_parameter("has_frame", false)
			if not panel.player.start(texture, material):
				push_error("Natural-completion replay was rejected")
				quit(1)
				return
			panel.view.material = material
			panel.video_material = material
			panel.video_texture = texture
		var status: String = panel.player.status()
		# Decoder/context startup is asynchronous and outside the four-second
		# clip. Check completion, not an assumed six-second startup+play duration.
		while not status.begins_with("Finished") and Time.get_ticks_msec() - started < 12000:
			await create_timer(0.1).timeout
			status = panel.player.status()
		# Wait for actual draws, not process ticks, to consume the final mailbox.
		var final_draw := draw_count + 2
		var drawing := Time.get_ticks_msec()
		while draw_count < final_draw and Time.get_ticks_msec() - drawing < 2000:
			await create_timer(0.05).timeout
		status = panel.player.status()
		print("WELD_VIDEO_STATUS ", status, " elapsed_ms=", Time.get_ticks_msec() - started,
			" draw_callbacks=", draw_count - first_draw)
		if not "decoded %d," % expected_frames in status or "presented 0," in status:
			push_error("Native fixture did not present (draw callbacks %d): %s" % [draw_count - first_draw, status])
			panel.stop()
			quit(1)
			return
		if attempt == 1:
			var capture: String = OS.get_environment("WELD_VR_VIDEO_CAPTURE")
			if not capture.is_empty():
				# Diagnostic only; decoded presentation never reads video to the CPU.
				var error := root.get_texture().get_image().save_png(capture)
				if error != OK:
					push_error("Could not save presentation screenshot")
					quit(1)
					return
		if attempt == 2:
			panel.stop()
		await create_timer(0.2).timeout
	main.queue_free()
	await process_frame
	print("WELD_NATIVE_VIDEO_SMOKE_OK")
	quit(0)
