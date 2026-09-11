extends Control
## Finite fixture UI; no network session or XR startup in this slice.

signal playback_started
signal playback_stopped

var player: WeldVideoPlayer
var video_texture: ExternalTexture
var video_material: ShaderMaterial
var status_elapsed := 0.0
@onready var view: ColorRect = $VideoFrame/Video


func _ready() -> void:
	player = WeldVideoPlayer.new()
	add_child(player)
	$Play.pressed.connect(play)
	$Stop.pressed.connect(stop)
	RenderingServer.frame_pre_draw.connect(_before_draw)
	# Explicit opt-in for the bounded physical presentation check.
	if "--video-fixture" in OS.get_cmdline_user_args():
		call_deferred("play")


func play() -> void:
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
	if not player.start(video_texture.get_external_texture_id(), video_material):
		view.material = null
		video_material = null
		video_texture = null
	else:
		playback_started.emit()
	$Status.text = player.status()


func stop() -> void:
	if not is_instance_valid(player):
		return
	# Rust also clears the sampler, so native Node teardown is safe independently
	# of GDScript child/parent exit order. Its cleanup finishes before ref release.
	player.stop()
	view.material = null
	video_material = null
	video_texture = null
	$Status.text = player.status()
	playback_stopped.emit()


func _before_draw() -> void:
	player.tick()


func _process(delta: float) -> void:
	status_elapsed += delta
	if status_elapsed >= 0.25:
		status_elapsed = 0.0
		$Status.text = player.status()


func _notification(what: int) -> void:
	if what == NOTIFICATION_APPLICATION_PAUSED:
		stop()
	elif what == NOTIFICATION_APPLICATION_RESUMED:
		# Resume never silently reconnects a producer or replaces the old context.
		stop()


func _exit_tree() -> void:
	if RenderingServer.frame_pre_draw.is_connected(_before_draw):
		RenderingServer.frame_pre_draw.disconnect(_before_draw)
	stop()
