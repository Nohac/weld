extends SceneTree


func _initialize() -> void:
	call_deferred("_run")


func _fail(message: String) -> void:
	push_error(message)
	quit(1)


func _run() -> void:
	if not ClassDB.class_exists("WeldBridge"):
		_fail("WeldBridge was not registered by the native library")
		return
	var packed: PackedScene = load(ProjectSettings.get_setting("application/run/main_scene"))
	if packed == null:
		_fail("Could not load the bridge scene")
		return
	var startup := packed.instantiate()
	root.add_child(startup)
	await process_frame
	var main := startup.get_child(0)
	if main.get_node_or_null("VideoPanel/VideoFrame/Video") == null or root.use_xr:
		_fail("Startup did not select the flat video-only scene without OpenXR")
		return
	var player: WeldVideoPlayer = main.get_node("VideoPanel").player
	for removed in ["key_input", "pointer_input", "reset_input", "take_cursor"]:
		if player.has_method(removed):
			_fail("Temporary scalar/dictionary input API is still exposed: " + removed)
			return
	if player.is_processing_input():
		_fail("An inactive player must disable input callbacks")
		return
	var key := InputEventKey.new()
	key.physical_keycode = KEY_A
	key.pressed = true
	var button := InputEventMouseButton.new()
	button.position = Vector2(10, 10)
	button.button_index = MOUSE_BUTTON_LEFT
	button.pressed = true
	var motion := InputEventMouseMotion.new()
	motion.position = Vector2(10, 10)
	# Presentation-only Controls must not override Rust's cursor with an arrow.
	# Check the real scene configuration, rather than repairing it in the test.
	for control in [main] + main.find_children("*", "Control"):
		if control.mouse_filter != Control.MOUSE_FILTER_IGNORE:
			_fail("Presentation control intercepts cursor/input: " + str(control.get_path()))
			return
	# Physics picking runs after Rust input. Exclude it from the inactive gate
	# assertion; GUI filtering remains real and would catch a STOP regression.
	root.physics_object_picking = false
	for event in [key, button, motion]:
		root.push_input(event)
		if root.is_input_handled():
			_fail("Inactive viewer consumed an engine input event: " + event.get_class())
			return
	key.pressed = false
	button.pressed = false
	root.push_input(key)
	root.push_input(button)
	for notification in [Node.NOTIFICATION_WM_WINDOW_FOCUS_OUT, Node.NOTIFICATION_WM_WINDOW_FOCUS_IN, Node.NOTIFICATION_WM_MOUSE_EXIT]:
		player.notification(notification)
	if player.is_processing_input():
		_fail("Focus notifications must not activate input without a live source")
		return
	var bridge := WeldBridge.new()
	main.add_child(bridge)
	for count in range(1, 3):
		if bridge.ping() != "Hello from Rust!\nTap count: %d" % count:
			_fail("Godot -> Rust response mismatch on call %d" % count)
			return
	startup.queue_free()
	await process_frame
	# A fresh instance must own fresh state after the previous scene is freed.
	startup = packed.instantiate()
	root.add_child(startup)
	await process_frame
	main = startup.get_child(0)
	bridge = WeldBridge.new()
	main.add_child(bridge)
	if bridge.ping() != "Hello from Rust!\nTap count: 1":
		_fail("Rust bridge state leaked between scene instances")
		return
	startup.queue_free()
	await process_frame
	var xr: Node = load("res://scenes/xr.tscn").instantiate()
	root.add_child(xr)
	await process_frame
	if not xr.get_node("XROrigin3D/XRCamera3D") is XRCamera3D:
		_fail("XR scene is missing its head-tracked camera")
		return
	var viewport: SubViewport = xr.get_node("PanelViewport")
	if xr.get_node("PanelViewport/VideoPanel").player.is_processing_input():
		_fail("XR video presentation must not enable desktop input")
		return
	var screen: MeshInstance3D = xr.get_node("Screen")
	if viewport.use_xr or screen.material_override.albedo_texture != viewport.get_texture():
		_fail("XR panel must sample the shared mono video viewport")
		return
	if xr.get_node_or_null("XROrigin3D/ControllerModels") != null:
		_fail("Runtime controller models must not instantiate without OpenXR")
		return
	for hand in ["LeftController", "RightController"]:
		var controller: XRController3D = xr.get_node("XROrigin3D/" + hand)
		var pointer: XRToolsFunctionPointer = controller.get_node("Pointer")
		if controller.visible or pointer.enabled or controller.pose != &"aim":
			_fail("Controller pointers must stay hidden without tracking and XR focus")
			return
		if not pointer.collision_mask & screen.get_node("PointerTarget").collision_layer:
			_fail("Controller pointer cannot hit the video panel")
			return
	xr.queue_free()
	await process_frame
	print("WELD_VR_GDEXTENSION_SMOKE_OK")
	quit(0)
