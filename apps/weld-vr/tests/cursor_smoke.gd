extends SceneTree
## Real display cursor selection against the production flat scene; no stream.


func _initialize() -> void:
	call_deferred("_run")


func _fail(message: String) -> void:
	Input.mouse_mode = Input.MOUSE_MODE_VISIBLE
	Input.set_default_cursor_shape(Input.CURSOR_ARROW)
	push_error(message)
	quit(1)


func _refresh_at(position: Vector2) -> void:
	# Match Godot 4.7's set_default_cursor_shape refresh. Internal events bypass
	# user _input callbacks and enter GUI cursor selection. push_input does not
	# update Input.mouse_pos, so supply the exact position after every request.
	var motion := InputEventMouseMotion.new()
	motion.device = -2 # InputEvent::DEVICE_ID_INTERNAL in core/input/input_event.h.
	motion.position = position
	motion.global_position = position
	root.push_input(motion, true)


func _run() -> void:
	if DisplayServer.get_name() == "headless" or not DisplayServer.has_feature(DisplayServer.FEATURE_CURSOR_SHAPE):
		_fail("Cursor selection test requires a real display with cursor shapes")
		return
	var main: Control = load("res://scenes/main.tscn").instantiate()
	root.add_child(main)
	await process_frame
	await process_frame
	var panel := main.get_node("VideoPanel")
	var view: Control = panel.get_node("VideoFrame/Video")
	var position := view.get_global_rect().get_center()
	if view.size.x <= 0.0 or view.size.y <= 0.0 or main.mouse_filter != Control.MOUSE_FILTER_IGNORE:
		_fail("Flat scene must have a nonempty image and ignore GUI mouse input")
		return
	# Deterministic GUI hover without moving the user's physical pointer.
	root.notification(Window.NOTIFICATION_VP_MOUSE_ENTER)
	for requested in [Input.CURSOR_ARROW, Input.CURSOR_HSIZE, Input.CURSOR_VSIZE, Input.CURSOR_IBEAM, Input.CURSOR_CROSS]:
		Input.set_default_cursor_shape(requested)
		_refresh_at(position)
		if Input.get_current_cursor_shape() != requested:
			_fail("Displayed cursor differs from requested shape %d" % requested)
			return
		print("WELD_CURSOR_SHAPE_OK ", requested)
	# Reproduce the bug, then restore and reassert the production configuration.
	main.mouse_filter = Control.MOUSE_FILTER_STOP
	Input.set_default_cursor_shape(Input.CURSOR_HSIZE)
	_refresh_at(position)
	if Input.get_current_cursor_shape() != Input.CURSOR_ARROW:
		_fail("Negative control did not reproduce the outer Control arrow override")
		return
	main.mouse_filter = Control.MOUSE_FILTER_IGNORE
	Input.set_default_cursor_shape(Input.CURSOR_HSIZE)
	_refresh_at(position)
	if Input.get_current_cursor_shape() != Input.CURSOR_HSIZE:
		_fail("Cursor did not recover after restoring the real scene")
		return
	Input.mouse_mode = Input.MOUSE_MODE_HIDDEN
	panel.player.stop()
	_refresh_at(position)
	if Input.get_current_cursor_shape() != Input.CURSOR_ARROW or Input.mouse_mode != Input.MOUSE_MODE_VISIBLE:
		_fail("Rust stop did not restore a visible arrow")
		return
	main.queue_free()
	await process_frame
	print("WELD_CURSOR_SMOKE_OK display=", DisplayServer.get_name())
	quit(0)
