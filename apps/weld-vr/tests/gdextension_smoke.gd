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
	var main := packed.instantiate()
	root.add_child(main)
	await process_frame
	var button := main.get_node_or_null("CanvasLayer/PingButton") as Button
	var label := main.get_node_or_null("Label3D") as Label3D
	if button == null or label == null:
		_fail("Missing bridge UI nodes")
		return
	for count in range(1, 3):
		button.pressed.emit()
		if label.text != "Hello from Rust!\nTap count: %d" % count:
			_fail("Godot -> Rust -> label response mismatch on press %d" % count)
			return
	main.queue_free()
	await process_frame
	# A fresh instance must own fresh state after the previous scene is freed.
	main = packed.instantiate()
	root.add_child(main)
	await process_frame
	main.get_node("CanvasLayer/PingButton").pressed.emit()
	if main.get_node("Label3D").text != "Hello from Rust!\nTap count: 1":
		_fail("Rust bridge state leaked between scene instances")
		return
	main.queue_free()
	await process_frame
	print("WELD_VR_GDEXTENSION_SMOKE_OK")
	quit(0)
