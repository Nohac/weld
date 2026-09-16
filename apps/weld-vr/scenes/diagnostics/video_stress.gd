extends Node3D
## Local decoder/presentation experiment. No Iroh, input forwarding, or host apps.
var entries: Array[Dictionary] = []
var elapsed := 0.0
var sample_elapsed := 0.0
var config := {"clip": "user://video-stress.ivf", "panels": 2, "seconds": 15,
	"width": 1920, "height": 1080, "tick": "predraw", "playout": "latest", "streams": []}
var finishing := false
var started_usec := 0


func _ready() -> void:
	if FileAccess.file_exists("user://video-stress.json"):
		var supplied = JSON.parse_string(FileAccess.get_file_as_string("user://video-stress.json"))
		DirAccess.remove_absolute(ProjectSettings.globalize_path("user://video-stress.json"))
		if supplied is Dictionary:
			config.merge(supplied, true)
	for argument in OS.get_cmdline_user_args():
		if argument.begins_with("--stress-config="):
			var supplied = JSON.parse_string(argument.trim_prefix("--stress-config="))
			if supplied is Dictionary:
				config.merge(supplied, true)
			continue
		if argument.begins_with("--stress-") and "=" in argument:
			var parts := argument.trim_prefix("--stress-").split("=", true, 1)
			config[parts[0]] = int(parts[1]) if parts[0] in ["panels", "seconds", "width", "height"] else parts[1]
	# Godot's JSON parser represents numbers as floats; normalize enum/count
	# fields before Array membership checks and native method calls.
	for key in ["panels", "seconds", "width", "height"]:
		config[key] = int(config[key])
	if not config.streams.is_empty():
		config.panels = config.streams.size()
	if config.panels < 1 or config.panels > 8 or config.seconds < 3 or config.seconds > 60 \
		or config.width < 320 or config.width > 3840 or config.height < 180 or config.height > 2160 \
		or config.tick not in ["predraw", "process", "postdraw"]:
		push_error("Invalid video stress configuration")
		get_tree().quit(1)
		return
	var xr := XRServer.find_interface("OpenXR") as OpenXRInterface
	var immersive := xr != null and xr.is_initialized()
	var origin: XROrigin3D
	DisplayServer.window_set_vsync_mode(DisplayServer.VSYNC_DISABLED)
	if immersive:
		xr.render_target_size_multiplier = 1.125
		get_viewport().use_xr = true
		get_viewport().msaa_3d = Viewport.MSAA_4X
		origin = XROrigin3D.new()
		add_child(origin)
		origin.add_child(XRCamera3D.new())
	else:
		Engine.max_fps = 90
		var camera := Camera3D.new()
		camera.position.y = 1.5
		add_child(camera)
		camera.current = true
	await get_tree().process_frame
	for index in range(config.panels):
		var spec: Dictionary = config.streams[index] if not config.streams.is_empty() else config
		var dimensions := Vector2i(int(spec.width), int(spec.height))
		if dimensions.x < 320 or dimensions.x > 3840 or dimensions.y < 180 or dimensions.y > 2160:
			push_error("Invalid stress stream extent")
			get_tree().quit(1)
			return
		var viewport := SubViewport.new()
		viewport.disable_3d = true
		viewport.size = dimensions
		viewport.render_target_update_mode = SubViewport.UPDATE_ALWAYS
		add_child(viewport)
		var panel = load("res://scenes/video_panel.tscn").instantiate()
		panel.auto_connect = false
		panel.spatial = true
		viewport.add_child(panel)
		if config.tick != "predraw":
			RenderingServer.frame_pre_draw.disconnect(panel._before_draw)
			if config.tick == "postdraw":
				RenderingServer.frame_post_draw.connect(panel._before_draw)
		var size := Vector2(1.5, 1.5 * float(dimensions.y) / float(dimensions.x))
		var position := Vector3((float(index) - float(config.panels - 1) / 2.0) * 1.55, 1.5, -2.5)
		if config.panels > 2:
			size *= 0.7 if index < 3 else 0.35
			position = Vector3((float(index) - 1.0) * 1.1, 1.7, -2.8) if index < 3 \
				else Vector3((float(index - 3) - 2.0) * 0.6, 0.9, -2.8)
		var native_layer: OpenXRCompositionLayerQuad
		if immersive:
			var layer := OpenXRCompositionLayerQuad.new()
			layer.visible = false
			origin.add_child(layer)
			if not layer.is_natively_supported():
				push_error("Stress test requires native composition layers in XR")
				get_tree().quit(1)
				return
			layer.layer_viewport = viewport
			layer.quad_size = size
			layer.position = position
			native_layer = layer
		else:
			var mesh := MeshInstance3D.new()
			var quad := QuadMesh.new()
			quad.size = size
			mesh.mesh = quad
			mesh.position = position
			var material := StandardMaterial3D.new()
			material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
			material.albedo_texture = viewport.get_texture()
			mesh.material_override = material
			add_child(mesh)
		entries.append({"panel": panel, "viewport": viewport, "layer": native_layer, "clip": spec.clip})
	# Opening a provider synchronizes rendering; never do that inside a
	# frame_post_draw signal with an XR swapchain image still acquired.
	_start_streams.call_deferred()


func _start_streams() -> void:
	for entry in entries:
		entry.panel._start(false, false, ProjectSettings.globalize_path(entry.clip), config.seconds, config.playout == "smooth")
	started_usec = Time.get_ticks_usec()
	print("WELD_STRESS_START ", JSON.stringify(config), " xr=", get_viewport().use_xr)


func _process(delta: float) -> void:
	if finishing or started_usec == 0:
		return
	elapsed = float(Time.get_ticks_usec() - started_usec) / 1000000.0
	sample_elapsed += delta
	for entry in entries:
		if entry.layer != null and not entry.layer.visible and entry.panel.player.diagnostic_stats().get("imported", 0) > 0:
			entry.layer.visible = true
		if config.tick == "process":
			entry.panel.player.tick()
	if sample_elapsed >= 1.0:
		sample_elapsed = 0.0
		_report("SAMPLE")
	if elapsed >= float(config.seconds) + 4.0:
		finishing = true
		_report("END")
		var failed := false
		for entry in entries:
			var stats: Dictionary = entry.panel.player.diagnostic_stats()
			if stats.get("decoded", 0) == 0 or stats.get("imported", 0) == 0 or "video:" in entry.panel.player.status():
				failed = true
			entry.panel.stop()
		print("WELD_STRESS_DONE success=", not failed)
		get_tree().quit(1 if failed else 0)


func _report(kind: String) -> void:
	for index in range(entries.size()):
		var stats: Dictionary = entries[index].panel.player.diagnostic_stats()
		stats["panel"] = index
		stats["elapsed"] = elapsed
		stats["engine_fps"] = Engine.get_frames_per_second()
		stats["status"] = entries[index].panel.player.status()
		print("WELD_STRESS_", kind, " ", JSON.stringify(stats))
