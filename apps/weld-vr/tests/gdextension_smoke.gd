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
	var frame: Control = main.get_node("VideoPanel/VideoFrame")
	var view: Control = frame.get_node("Video")
	if frame is Container:
		_fail("Video geometry must not depend on deferred Container layout")
		return
	player.layout_video(view, Vector2(1000, 1000), false)
	if not view.position.is_equal_approx(Vector2(0, 218.75)) or not view.size.is_equal_approx(Vector2(1000, 562.5)):
		_fail("Flat letterbox fit must update synchronously")
		return
	player.layout_video(view, Vector2(600, 1000), true)
	if not view.position.is_zero_approx() or not view.size.is_equal_approx(Vector2(600, 1000)):
		_fail("Spatial video must fill the exact analytical input rectangle")
		return
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
	if not xr.use_native_panel or xr.composition_panel != null:
		_fail("XR must prefer native layers but not instantiate them without OpenXR")
		return
	var video_panel := xr.get_node("PanelViewport/VideoPanel")
	if video_panel.auto_connect or not video_panel.spatial:
		_fail("XR must resolve headset preferences before connecting, with spatial layout")
		return
	video_panel._process(0.3)
	if not "Waiting for headset" in video_panel.get_node("Status").text:
		_fail("Headset waiting status must survive the periodic status refresh")
		return
	var projections: Array[Projection] = [Projection.create_perspective(90.0, 1.0, 0.05, 100.0)]
	if not video_panel.player.configure_xr_presentation(Vector2(2160, 2160), projections,
		Vector2(1.6, 1.0), 1.6, 1.8, 2.0) or video_panel.player.xr_viewport_size() != Vector2i.ZERO:
		_fail("XR raster sizing must wait for actual content, not latch the default aspect")
		return
	viewport.size = Vector2i(800, 1200)
	video_panel.layout_video()
	if video_panel.view.size != Vector2(800, 1200) or not video_panel.view.position.is_zero_approx():
		_fail("XR viewport resize must immediately update visible and input bounds")
		return
	if xr.get_node("PanelViewport/VideoPanel").player.is_processing_input():
		_fail("XR video presentation must not enable desktop input")
		return
	var screen: MeshInstance3D = xr.get_node("Screen")
	var environment: Environment = xr.get_node("WorldEnvironment").environment
	var light: DirectionalLight3D = xr.get_node("ControllerLight")
	if environment.ambient_light_source != Environment.AMBIENT_SOURCE_COLOR \
		or environment.ambient_light_energy <= 0.0 or light.light_energy <= 0.0:
		_fail("XR controller models need ambient and directional illumination")
		return
	if screen.material_override.shading_mode != BaseMaterial3D.SHADING_MODE_UNSHADED:
		_fail("Controller lighting must not shade the streamed video")
		return
	if viewport.use_xr or screen.material_override.albedo_texture != viewport.get_texture():
		_fail("XR panel must sample the shared mono video viewport")
		return
	if xr.get_node_or_null("XROrigin3D/ControllerModels") != null \
		or xr.get_node_or_null("XROrigin3D/RightControllerRig/ControllerModels") != null:
		_fail("Runtime controller models must not instantiate without OpenXR")
		return
	var controller: XRController3D = xr.get_node("XROrigin3D/RightControllerRig/Aim")
	var pointer: WeldXrPointer = controller.get_node("PointerTilt/Pointer")
	if pointer.visible or controller.pose != &"aim" or controller.tracker != &"right_hand":
		_fail("Rust pointer must stay hidden without an active tracked XR session")
		return
	for removed in ["key_input", "pointer_input", "reset_input", "take_cursor"]:
		if pointer.has_method(removed):
			_fail("XR pointer must not expose a scripted scalar input API")
			return
	if xr.has_node("XROrigin3D/LeftController/Pointer") or screen.has_node("PointerTarget"):
		_fail("Single Rust pointer must not retain an alternate collider/input path")
		return
	if xr.process_priority != 100 or pointer.process_priority != 200:
		_fail("Rig correction must precede pointer sampling")
		return
	if not _check_controller_rig(xr):
		return
	var parent_logical := Vector2(800, 600)
	var parent_size := Vector2(1.6, 1.2)
	for logical in [Vector2(800, 600), Vector2(600, 1200), Vector2(100, 50)]:
		var fitted: Vector2 = xr._secondary_size(logical, parent_logical, parent_size)
		if fitted.x > parent_size.x * 0.75 + 0.00001 or fitted.y > parent_size.y * 0.75 + 0.00001 \
			or not is_equal_approx(fitted.x / fitted.y, logical.x / logical.y):
			_fail("Secondary XR windows must preserve aspect and leave the primary visible")
			return
	if not xr._secondary_size(Vector2(100, 50), parent_logical, parent_size).is_equal_approx(Vector2(0.2, 0.1)):
		_fail("Small secondary XR windows must not be enlarged to the size limit")
		return
	# Visibility changes register/unregister native composition layers. A
	# steady layout must not hide and re-show a layer every frame.
	var probe_mesh := MeshInstance3D.new()
	var probe_layer := Node3D.new()
	probe_mesh.visible = false
	probe_layer.visible = false
	xr.add_child(probe_mesh)
	xr.add_child(probe_layer)
	var visibility_changes := [0]
	probe_layer.visibility_changed.connect(func(): visibility_changes[0] += 1)
	var entry := {"mesh": probe_mesh, "layer": probe_layer, "stereo": null}
	for _frame in range(5):
		xr._set_entry_visible(entry, true)
	if visibility_changes[0] != 1:
		_fail("A visible XR layer must remain registered across steady frames")
		return
	xr._set_entry_visible(entry, false)
	xr._set_entry_visible(entry, false)
	if visibility_changes[0] != 2:
		_fail("An XR layer must hide only once on unmap")
		return
	xr.queue_free()
	await process_frame
	print("WELD_VR_GDEXTENSION_SMOKE_OK")
	quit(0)


func _check_controller_rig(xr: Node) -> bool:
	var rig = xr.get_node("XROrigin3D/RightControllerRig")
	if rig.origin_offset_enabled:
		_fail("Ordinary desktop/Phone features must not enable the Pico correction")
		return false
	var tracker := XRControllerTracker.new()
	tracker.name = &"right_hand"
	XRServer.add_tracker(tracker)
	var grip_pose := Transform3D(Basis.from_euler(Vector3(0.2, 0.1, 0)), Vector3(0.1, 1.2, -0.5))
	var aim_pose := Transform3D(Basis.from_euler(Vector3(-0.2, 0.1, 0)), Vector3(0.08, 1.17, -0.56))
	tracker.set_pose(&"grip", grip_pose, Vector3.ZERO, Vector3.ZERO, XRPose.XR_TRACKING_CONFIDENCE_HIGH)
	tracker.set_pose(&"aim", aim_pose, Vector3.ZERO, Vector3.ZERO, XRPose.XR_TRACKING_CONFIDENCE_HIGH)
	var model := Node3D.new()
	model.transform = Transform3D(Basis.from_euler(Vector3(0.3, 0.1, 0)), Vector3(0.12, 1.21, -0.49))
	rig.add_child(model)
	var original_model := model.transform
	rig.origin_offset_enabled = true
	var delta := aim_pose.origin - grip_pose.origin
	for _iteration in range(3):
		rig.update_tracking(true)
		if not rig.visible or not rig.position.is_equal_approx(delta):
			return _rig_fail("Tracked rig must stay visible with a non-accumulating aim/grip offset", tracker, model)
		if not model.transform.is_equal_approx(original_model):
			return _rig_fail("Rig correction must preserve the model local transform", tracker, model)
		if not model.global_position.is_equal_approx(original_model.origin + delta):
			return _rig_fail("Rig correction must move the model globally by the shared offset", tracker, model)
		if not (rig.grip.global_transform.affine_inverse() * rig.aim.global_transform).is_equal_approx(grip_pose.affine_inverse() * aim_pose):
			return _rig_fail("Rig correction must preserve relative grip/aim poses", tracker, model)
	if not is_equal_approx(rig.pointer_tilt.rotation.x, -deg_to_rad(5.0)):
		return _rig_fail("Default tilt must be negative five degrees around local X", tracker, model)
	var ray_in_aim: Vector3 = rig.aim.global_basis.inverse() * -rig.pointer_tilt.global_basis.z
	if ray_in_aim.y >= 0.0:
		return _rig_fail("Positive ergonomic tilt must point down in aim space", tracker, model)
	rig.pointer_tilt_degrees = 100.0
	rig.update_tracking(true)
	if not is_equal_approx(rig.pointer_tilt.rotation.x, -deg_to_rad(30.0)):
		return _rig_fail("Positive tilt must clamp to thirty degrees", tracker, model)
	rig.pointer_tilt_degrees = -100.0
	rig.update_tracking(true)
	if not is_equal_approx(rig.pointer_tilt.rotation.x, deg_to_rad(30.0)):
		return _rig_fail("Negative tilt must clamp to minus thirty degrees", tracker, model)
	rig.pointer_tilt_degrees = NAN
	rig.update_tracking(true)
	if not rig.pointer_tilt.rotation.is_zero_approx():
		return _rig_fail("Non-finite tilt must fall back to zero", tracker, model)
	rig.pointer_tilt_degrees = 5.0
	tracker.invalidate_pose(&"grip")
	rig.update_tracking(true)
	if rig.visible or not rig.position.is_zero_approx():
		return _rig_fail("Grip loss must hide and reset a corrected rig", tracker, model)
	rig.origin_offset_enabled = false
	rig.update_tracking(true)
	if not rig.visible or not rig.position.is_zero_approx():
		return _rig_fail("Uncorrected rig must permit aim-only tracking with no offset", tracker, model)
	tracker.invalidate_pose(&"aim")
	rig.update_tracking(true)
	if rig.visible:
		return _rig_fail("Aim loss must hide the rig", tracker, model)
	tracker.set_pose(&"aim", aim_pose, Vector3.ZERO, Vector3.ZERO, XRPose.XR_TRACKING_CONFIDENCE_HIGH)
	tracker.set_pose(&"grip", grip_pose, Vector3.ZERO, Vector3.ZERO, XRPose.XR_TRACKING_CONFIDENCE_HIGH)
	rig.origin_offset_enabled = true
	rig.update_tracking(true)
	if not rig.visible or not rig.position.is_equal_approx(delta):
		return _rig_fail("Restored tracking must recover the corrected rig", tracker, model)
	rig.update_tracking(false)
	if rig.visible or not rig.position.is_zero_approx():
		return _rig_fail("Focus loss must hide and reset the rig", tracker, model)
	XRServer.remove_tracker(tracker)
	model.queue_free()
	return true


func _rig_fail(message: String, tracker: XRControllerTracker, model: Node3D) -> bool:
	XRServer.remove_tracker(tracker)
	model.queue_free()
	_fail(message)
	return false
