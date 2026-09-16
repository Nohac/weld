extends Control
## One GPU-native live window, with the finite AV1 fixture retained for diagnosis.

signal playback_started
signal playback_stopped

@export var desktop_input := false
@export var auto_connect := true
@export var spatial := false

var player: WeldVideoPlayer
var video_texture: ExternalTexture
var video_material: ShaderMaterial
var status_elapsed := 0.0
var network_active := false
var log_elapsed := 0.0
var waiting_message := ""
var window_controls := {}
@onready var view: ColorRect = $VideoFrame/Video


func _ready() -> void:
	process_priority = 50
	player = WeldVideoPlayer.new()
	add_child(player)
	player.configure_input(view, desktop_input)
	RenderingServer.frame_pre_draw.connect(_before_draw)
	# Explicit opt-in for the bounded physical presentation check.
	if "--video-fixture" in OS.get_cmdline_user_args() or "--video-single-frame" in OS.get_cmdline_user_args():
		call_deferred("play")
	elif auto_connect and DisplayServer.get_name() != "headless" and "--script" not in OS.get_cmdline_args():
		call_deferred("connect_source")


func play() -> void:
	_start(false, "--video-single-frame" in OS.get_cmdline_user_args())


func connect_source() -> void:
	_start(true)


func _start(network: bool, single_frame: bool = false, stress_path: String = "", stress_seconds: int = 15, stress_smooth: bool = false) -> void:
	waiting_message = ""
	stop()
	if Engine.is_editor_hint() or DisplayServer.get_name() == "headless":
		$Status.text = "Native video requires a running EGL display (not editor/headless)."
		return
	# A fresh texture per session. Resizing ExternalTexture after native import
	# would replace its storage underneath the retained decoder image.
	video_texture = ExternalTexture.new()
	video_texture.size = Vector2(320, 180)
	var shader := Shader.new()
	var sampler := "samplerExternalOES" if OS.get_name() == "Android" else "sampler2D"
	shader.code = """shader_type canvas_item;
uniform %s video;
uniform vec4 crop = vec4(0.0, 0.0, 1.0, 1.0);
uniform bool has_frame = false;
void fragment() {
	if (has_frame) {
		COLOR = vec4(texture(video, mix(crop.xy, crop.zw, UV)).rgb, 1.0);
	} else { COLOR = vec4(0.04, 0.04, 0.04, 1.0); }
}
""" % sampler
	video_material = ShaderMaterial.new()
	video_material.shader = shader
	video_material.set_shader_parameter("video", video_texture)
	view.material = video_material
	var started: bool
	if network:
		var refresh := _presenter_refresh()
		var rate := _valid_millihertz(refresh.selected)
		if rate == 0:
			rate = 60000
		var directory := OS.get_environment("WELD_VR_DEVICE_DIR")
		if directory.is_empty():
			directory = ProjectSettings.globalize_path("user://weld-device")
		started = player.start_stream(video_texture, video_material,
			directory, rate)
		if started:
			_log_refresh(refresh, rate)
	else:
		if not stress_path.is_empty():
			started = player.start_stress(video_texture, video_material, stress_path, stress_seconds, stress_smooth)
		elif single_frame:
			started = player.start_single_frame(video_texture, video_material)
		else:
			started = player.start(video_texture, video_material)
	if not started:
		view.material = null
		video_material = null
		video_texture = null
	else:
		network_active = network
		playback_started.emit()
	$Status.text = player.status()
	$Status.show()


func stop() -> void:
	network_active = false
	if not is_instance_valid(player):
		return
	# Rust also clears the sampler, so native Node teardown is safe independently
	# of GDScript child/parent exit order. Its cleanup finishes before ref release.
	player.stop()
	view.material = null
	video_material = null
	video_texture = null
	$Status.text = player.status()
	$Status.show()
	playback_stopped.emit()


func _before_draw() -> void:
	player.tick()


func _presenter_refresh() -> Dictionary:
	var flat := DisplayServer.screen_get_refresh_rate()
	var xr := XRServer.primary_interface
	var immersive := xr != null and xr.is_initialized()
	var headset: float = xr.get_display_refresh_rate() if immersive else 0.0
	return {"screen": flat, "xr": headset, "selected": headset if immersive else flat}


func _valid_millihertz(refresh: float) -> int:
	if not is_finite(refresh) or refresh < 1.0 or refresh > 1000.0:
		return 0
	return roundi(refresh * 1000.0)


func _log_refresh(sample: Dictionary, rate: int) -> void:
	print("WELD_PRESENTER_RATE screen_hz=", sample.screen,
		" xr_hz=", sample.xr, " selected_millihertz=", rate)


func _process(delta: float) -> void:
	layout_video()
	if network_active and not spatial:
		_layout_windows()
	status_elapsed += delta
	if status_elapsed >= 0.25:
		status_elapsed = 0.0
		if network_active:
			var refresh := _presenter_refresh()
			var rate := _valid_millihertz(refresh.selected)
			if rate != 0 and player.set_presenter_rate(rate):
				_log_refresh(refresh, rate)
		$Status.text = waiting_message if not waiting_message.is_empty() else player.status()
		$Status.visible = not _has_window() if network_active else video_material == null or not video_material.get_shader_parameter("has_frame")
	if network_active or "--video-single-frame" in OS.get_cmdline_user_args():
		log_elapsed += delta
		if log_elapsed >= 1.0:
			log_elapsed = 0.0
			print("WELD_HOIST_STATUS ", player.status())


func layout_video() -> void:
	if network_active:
		view.hide()
		return
	view.show()
	var bounds := Vector2(get_viewport().size) if spatial else size
	player.layout_video(view, bounds, spatial)


func _has_window() -> bool:
	for surface in player.surfaces():
		if surface.is_mapped():
			return true
	return false


func _layout_windows() -> void:
	var surfaces := player.surfaces()
	var live := {}
	var roots := {}
	var independent: Array = []
	for surface in surfaces:
		var key := surface.surface_id()
		live[key] = true
		if not window_controls.has(key):
			var control := ColorRect.new()
			control.mouse_filter = Control.MOUSE_FILTER_IGNORE
			control.material = surface.video_material()
			add_child(control)
			surface.bind_control(control)
			window_controls[key] = control
		window_controls[key].visible = surface.is_mapped()
		if surface.kind() != 3:
			roots[surface.window_id()] = surface
		if surface.kind() == 0 and surface.is_mapped():
			independent.append(surface.window_id())
	for key in window_controls.keys():
		if not live.has(key):
			window_controls[key].queue_free()
			window_controls.erase(key)
	var placed := {}
	for _pass in range(8):
		for surface in surfaces:
			var key := surface.surface_id()
			if placed.has(key) or not surface.is_mapped():
				continue
			var control: ColorRect = window_controls[key]
			var logical := surface.logical_size()
			if surface.kind() == 0:
				var slot := Vector2(size.x / maxi(1, independent.size()), size.y)
				var factor := minf(slot.x / logical.x, slot.y / logical.y)
				control.size = logical * factor
				control.position = Vector2(slot.x * independent.find(surface.window_id()), 0) + (slot - control.size) * 0.5
			else:
				var parent = roots.get(surface.parent_id())
				if parent == null or not placed.has(parent.surface_id()):
					continue
				var parent_control: ColorRect = window_controls[parent.surface_id()]
				var factor: float = parent_control.size.x / parent.logical_size().x
				control.size = logical * factor
				control.position = parent_control.position + ((parent_control.size - control.size) * 0.5 if surface.kind() == 1 else surface.logical_position() * factor)
			placed[key] = true
			move_child(control, -1)


func _notification(what: int) -> void:
	if what == NOTIFICATION_APPLICATION_PAUSED:
		stop()
	elif what == NOTIFICATION_APPLICATION_RESUMED:
		# Relaunch the viewer/test after pause. Rejoining the same source still
		# needs source-side re-admission; do not silently restart its producer.
		stop()


func _exit_tree() -> void:
	if RenderingServer.frame_pre_draw.is_connected(_before_draw):
		RenderingServer.frame_pre_draw.disconnect(_before_draw)
	stop()
