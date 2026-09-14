extends Node3D

var xr: OpenXRInterface


func _ready() -> void:
	xr = XRServer.find_interface("OpenXR")
	if xr == null or not xr.is_initialized():
		push_error("BASELINE requires OpenXR")
		return
	var origin := XROrigin3D.new()
	add_child(origin)
	var camera := XRCamera3D.new()
	origin.add_child(camera)
	camera.current = true
	camera.near = 0.05
	var environment := Environment.new()
	environment.background_mode = Environment.BG_CLEAR_COLOR
	environment.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	environment.ambient_light_color = Color.WHITE
	environment.ambient_light_energy = 0.6
	environment.ambient_light_sky_contribution = 0.0
	var world := WorldEnvironment.new()
	world.environment = environment
	add_child(world)
	var light := DirectionalLight3D.new()
	light.rotation = Vector3(-0.8, -0.5, 0)
	light.light_energy = 1.2
	add_child(light)
	xr.render_target_size_multiplier = 1.125
	get_viewport().msaa_3d = Viewport.MSAA_4X
	get_viewport().use_xr = true
	DisplayServer.window_set_vsync_mode(DisplayServer.VSYNC_DISABLED)
	xr.session_begun.connect(_passthrough)
	_passthrough()
	var models := OpenXRRenderModelManager.new()
	origin.add_child(models)
	print("WELD_XR_MINIMAL native Godot only - no GDExtension, video, input forwarding or model overrides")
	get_tree().create_timer(5.0).timeout.connect(_report)


func _report() -> void:
	var tracker := XRServer.get_tracker("right_hand") as XRPositionalTracker
	var report := {"profile": str(tracker.profile) if tracker != null else "missing", "renderer": RenderingServer.get_current_rendering_method()}
	var file := FileAccess.open("user://baseline-state.json", FileAccess.WRITE)
	if file != null:
		file.store_string(JSON.stringify(report))
		file.close()
	print("WELD_XR_MINIMAL ", report)


func _passthrough() -> void:
	var enabled := xr.set_environment_blend_mode(XRInterface.XR_ENV_BLEND_MODE_ALPHA_BLEND)
	get_viewport().transparent_bg = enabled
	print("WELD_XR_MINIMAL passthrough=", enabled)
