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
	var screen: MeshInstance3D = xr.get_node("Screen")
	if viewport.use_xr or screen.material_override.albedo_texture != viewport.get_texture():
		_fail("XR panel must sample the shared mono video viewport")
		return
	xr.queue_free()
	await process_frame
	print("WELD_VR_GDEXTENSION_SMOKE_OK")
	quit(0)
