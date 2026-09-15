extends SceneTree
## Real EGL regression: independent textures, overlapping lifetimes and removal.


func _initialize() -> void:
	call_deferred("_run")


func _run() -> void:
	var scene: PackedScene = load("res://scenes/video_panel.tscn")
	var first := scene.instantiate()
	var second := scene.instantiate()
	first.auto_connect = false
	second.auto_connect = false
	root.add_child(first)
	root.add_child(second)
	await process_frame
	first.size = Vector2(600, 400)
	second.size = Vector2(600, 400)
	second.position = Vector2(600, 0)
	await RenderingServer.frame_post_draw
	first._start(false, true)
	second._start(false, false)
	var deadline := Time.get_ticks_msec() + 12000
	while not "presented 1," in first.player.status() and Time.get_ticks_msec() < deadline:
		await create_timer(0.05).timeout
	if not "presented 1," in first.player.status():
		push_error("First simultaneous presenter did not display: " + first.player.status())
		quit(1)
		return
	first.stop()
	first.queue_free()
	while not "decoded 120," in second.player.status() and Time.get_ticks_msec() < deadline:
		await create_timer(0.05).timeout
	await RenderingServer.frame_post_draw
	await RenderingServer.frame_post_draw
	print("WELD_MULTI_VIDEO_STATUS ", second.player.status())
	if not "decoded 120," in second.player.status() or "presented 0," in second.player.status():
		push_error("Closing a presenter interrupted its sibling")
		quit(1)
		return
	second.stop()
	second.queue_free()
	await process_frame
	print("WELD_MULTI_VIDEO_SMOKE_OK")
	quit(0)
