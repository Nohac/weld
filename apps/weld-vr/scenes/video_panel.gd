extends Control
## One GPU-native live window, with the finite AV1 fixture retained for diagnosis.

signal playback_started
signal playback_stopped

@export var desktop_input := false

var player: WeldVideoPlayer
var video_texture: ExternalTexture
var video_material: ShaderMaterial
var status_elapsed := 0.0
var network_active := false
var log_elapsed := 0.0
@onready var view: ColorRect = $VideoFrame/Video


func _ready() -> void:
	player = WeldVideoPlayer.new()
	add_child(player)
	player.configure_input(view, desktop_input)
	RenderingServer.frame_pre_draw.connect(_before_draw)
	# Explicit opt-in for the bounded physical presentation check.
	if "--video-fixture" in OS.get_cmdline_user_args() or "--video-single-frame" in OS.get_cmdline_user_args():
		call_deferred("play")
	elif DisplayServer.get_name() != "headless" and "--script" not in OS.get_cmdline_args():
		call_deferred("connect_source")


func play() -> void:
	_start(false, "--video-single-frame" in OS.get_cmdline_user_args())


func connect_source() -> void:
	_start(true)


func _start(network: bool, single_frame: bool = false) -> void:
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
		var refresh := DisplayServer.screen_get_refresh_rate()
		var xr := XRServer.primary_interface
		if xr != null and xr.is_initialized():
			refresh = xr.get_display_refresh_rate()
		if refresh <= 0.0:
			refresh = 60.0
		var directory := OS.get_environment("WELD_VR_DEVICE_DIR")
		if directory.is_empty():
			directory = ProjectSettings.globalize_path("user://weld-device")
		started = player.start_stream(video_texture, video_material,
			directory, int(clampf(refresh, 1.0, 1000.0) * 1000.0))
	else:
		if single_frame:
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


func _process(delta: float) -> void:
	status_elapsed += delta
	if status_elapsed >= 0.25:
		status_elapsed = 0.0
		$Status.text = player.status()
		$VideoFrame.ratio = player.aspect()
		$Status.visible = video_material == null or not video_material.get_shader_parameter("has_frame")
	if network_active or "--video-single-frame" in OS.get_cmdline_user_args():
		log_elapsed += delta
		if log_elapsed >= 1.0:
			log_elapsed = 0.0
			print("WELD_HOIST_STATUS ", player.status())


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
