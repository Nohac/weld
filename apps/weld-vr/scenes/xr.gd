extends Node3D
## Owns headset presentation, not the video receiver or native image lifetime.

@export_range(0.5, 2.0, 0.025) var eye_render_scale := 1.125
@export var panel_texture_filter: BaseMaterial3D.TextureFilter = BaseMaterial3D.TEXTURE_FILTER_LINEAR_WITH_MIPMAPS_ANISOTROPIC

var xr_interface: OpenXRInterface
var placement_pending := true
var passthrough_active := false

@onready var camera: XRCamera3D = $XROrigin3D/XRCamera3D
@onready var screen: MeshInstance3D = $Screen
@onready var panel: Control = $PanelViewport/VideoPanel


func _ready() -> void:
	var material := StandardMaterial3D.new()
	material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	# GLES falls back to level-zero filtering for this non-mipmapped viewport;
	# anisotropy can still filter the footprint when viewing the panel obliquely.
	material.texture_filter = panel_texture_filter
	material.albedo_texture = $PanelViewport.get_texture()
	screen.material_override = material
	xr_interface = XRServer.find_interface("OpenXR") as OpenXRInterface
	if xr_interface == null or not xr_interface.is_initialized():
		# Scene inspection/tests can load the hierarchy without an XR runtime.
		return
	# Set before the first XR viewport draw. On the tested Pico, 1920 * 1.125
	# gives a 2160-pixel eye target; this does not upscale the source video.
	xr_interface.render_target_size_multiplier = eye_render_scale
	DisplayServer.window_set_vsync_mode(DisplayServer.VSYNC_DISABLED)
	get_viewport().msaa_3d = Viewport.MSAA_4X
	get_viewport().use_xr = true
	xr_interface.session_begun.connect(_configure_passthrough)
	xr_interface.session_begun.connect(_queue_render_diagnostics)
	xr_interface.pose_recentered.connect(_recenter)
	xr_interface.session_stopping.connect(_stop)
	xr_interface.session_loss_pending.connect(_stop)
	xr_interface.instance_exiting.connect(_exit_xr)
	_configure_passthrough()
	_queue_render_diagnostics()


func _queue_render_diagnostics() -> void:
	# Sample after drawing, when the XR viewport has acquired its render target.
	# Reading texture dimensions does not read back any pixels from the GPU.
	if not RenderingServer.frame_post_draw.is_connected(_log_render_target):
		RenderingServer.frame_post_draw.connect(_log_render_target, CONNECT_ONE_SHOT)


func _log_render_target() -> void:
	if xr_interface == null or not xr_interface.is_initialized():
		return
	var viewport := get_viewport()
	print("WELD_XR_RENDER eye_target=", xr_interface.get_render_target_size(),
		" views=", xr_interface.get_view_count(),
		" xr_size_multiplier=", xr_interface.render_target_size_multiplier,
		" viewport_texture=", viewport.get_texture().get_size(),
		" viewport_3d_scale=", viewport.scaling_3d_scale,
		" msaa_3d=", viewport.msaa_3d,
		" panel_texture=", $PanelViewport.size,
		" panel_filter=", screen.material_override.texture_filter,
		" refresh_hz=", xr_interface.display_refresh_rate)


func _configure_passthrough() -> void:
	# Alpha blend is the OpenXR compositor's camera background, not raw camera
	# access. No vendor SDK or camera-feed permission is needed by this scene.
	var modes := xr_interface.get_supported_environment_blend_modes()
	passthrough_active = false
	if XRInterface.XR_ENV_BLEND_MODE_ALPHA_BLEND in modes:
		passthrough_active = xr_interface.set_environment_blend_mode(
			XRInterface.XR_ENV_BLEND_MODE_ALPHA_BLEND)
	if not passthrough_active:
		xr_interface.set_environment_blend_mode(XRInterface.XR_ENV_BLEND_MODE_OPAQUE)
	get_viewport().transparent_bg = passthrough_active
	print("WELD_XR passthrough=", passthrough_active, " blend_modes=", modes)


func _process(_delta: float) -> void:
	if not placement_pending or xr_interface == null or not xr_interface.is_initialized():
		return
	var head := XRServer.get_tracker("head") as XRPositionalTracker
	if head == null:
		return
	var pose := head.get_pose("default")
	if pose == null or not pose.has_tracking_data:
		return
	# Place once from a valid pose; normal head movement never drags the panel.
	var forward := -camera.global_basis.z
	forward.y = 0.0
	if forward.length_squared() < 0.001:
		return
	forward = forward.normalized()
	screen.global_position = camera.global_position + forward * 1.6 + Vector3.DOWN * 0.15
	screen.global_basis = Basis.looking_at(forward).rotated(forward.cross(Vector3.UP), -0.12)
	screen.show()
	placement_pending = false
	print("WELD_XR panel placed from tracked head pose")


func _recenter() -> void:
	placement_pending = true


func _stop() -> void:
	panel.stop()


func _exit_xr() -> void:
	_stop()
	get_tree().quit()


func _exit_tree() -> void:
	if xr_interface != null and xr_interface.is_initialized():
		if passthrough_active:
			xr_interface.stop_passthrough()
		get_viewport().transparent_bg = false
		get_viewport().use_xr = false
