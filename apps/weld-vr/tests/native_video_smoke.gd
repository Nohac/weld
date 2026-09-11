extends SceneTree
## Run with a real EGL display, never headless. Saves one diagnostic screenshot.


func _initialize() -> void:
	call_deferred("_run")


func _run() -> void:
	var scene: PackedScene = load("res://scenes/main.tscn")
	var main := scene.instantiate()
	root.add_child(main)
	await process_frame
	var panel := main.get_node("CanvasLayer/VideoPanel")
	# Keep manual input from restarting a clip while automation owns the session.
	panel.get_node("Play").disabled = true
	panel.get_node("Stop").disabled = true
	for attempt in range(2):
		var started := Time.get_ticks_msec()
		if attempt == 0:
			panel.play()
		else:
			# A naturally completed native session must accept a fresh target
			# without relying on the UI's usual explicit stop-before-play.
			var texture := ExternalTexture.new()
			texture.size = Vector2(320, 180)
			var material := panel.video_material.duplicate() as ShaderMaterial
			material.set_shader_parameter("video", texture)
			material.set_shader_parameter("has_frame", false)
			if not panel.player.start(texture.get_external_texture_id(), material):
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
		# Allow the final decoded mailbox image to reach the next render callback.
		await process_frame
		await process_frame
		status = panel.player.status()
		print("WELD_VIDEO_STATUS ", status, " elapsed_ms=", Time.get_ticks_msec() - started)
		if not "decoded 120" in status or "presented 0," in status:
			push_error("Native fixture did not present: " + status)
			panel.stop()
			quit(1)
			return
		if attempt == 0:
			var capture: String = OS.get_environment("WELD_VR_VIDEO_CAPTURE")
			if not capture.is_empty():
				# Diagnostic only; decoded presentation never reads video to the CPU.
				var error := root.get_texture().get_image().save_png(capture)
				if error != OK:
					push_error("Could not save presentation screenshot")
					quit(1)
					return
		if attempt == 1:
			panel.stop()
		await create_timer(0.2).timeout
	main.queue_free()
	await process_frame
	print("WELD_NATIVE_VIDEO_SMOKE_OK")
	quit(0)
