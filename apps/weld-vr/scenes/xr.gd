extends Node3D
## Owns headset presentation, not the video receiver or native image lifetime.

@export_range(0.5, 2.0, 0.025) var eye_render_scale := 1.125
@export var panel_texture_filter: BaseMaterial3D.TextureFilter = BaseMaterial3D.TEXTURE_FILTER_LINEAR_WITH_MIPMAPS_ANISOTROPIC
@export_range(1.0, 3.0, 0.05) var application_scale := 1.8
@export_range(0.5, 3.0, 0.1) var panel_sampling := 2.0
@export var panel_envelope := Vector2(1.6, 1.0)
@export_range(0.2, 10.0, 0.1) var panel_distance := 1.6
@export var use_native_panel := true

var xr_interface: OpenXRInterface
var placement_pending := true
var passthrough_active := false
var controller_models: Array[OpenXRRenderModelManager] = []
var preferences_resolved := false
var preferences_wait_started := Time.get_ticks_msec()
var composition_panel: OpenXRCompositionLayerQuad

const ControllerRig = preload("res://scenes/controller_rig.gd")

@onready var camera: XRCamera3D = $XROrigin3D/XRCamera3D
@onready var screen: MeshInstance3D = $Screen
@onready var panel: Control = $PanelViewport/VideoPanel
@onready var right_rig: ControllerRig = $XROrigin3D/RightControllerRig


func _ready() -> void:
	panel.waiting_message = "Waiting for headset focus and tracking"
	# Native pose/model updates run at 0; Rust pointer sampling runs at 200.
	# Apply presentation transforms before both hit testing and renderer flush.
	process_priority = 100
	var material := StandardMaterial3D.new()
	material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	# GLES falls back to level-zero filtering for this non-mipmapped viewport;
	# anisotropy can still filter the footprint when viewing the panel obliquely.
	material.texture_filter = panel_texture_filter
	material.albedo_texture = $PanelViewport.get_texture()
	screen.material_override = material
	$XROrigin3D/RightControllerRig/Aim/PointerTilt/Pointer.configure(panel.player,
		right_rig.aim, screen, panel.view)
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
	_setup_panel_presentation()
	xr_interface.session_begun.connect(_configure_passthrough)
	xr_interface.session_begun.connect(_queue_render_diagnostics)
	xr_interface.pose_recentered.connect(_recenter)
	xr_interface.session_stopping.connect(_stop)
	xr_interface.session_loss_pending.connect(_stop)
	xr_interface.instance_exiting.connect(_exit_xr)
	_configure_passthrough()
	_queue_render_diagnostics()
	if Engine.has_singleton("OpenXRRenderModelExtension"):
		# Godot's manager requires the initialized extension even in its
		# constructor, so do not instantiate it during non-XR scene inspection.
		_add_controller_models($XROrigin3D, OpenXRRenderModelManager.RENDER_MODEL_TRACKER_LEFT_HAND)
		_add_controller_models(right_rig, OpenXRRenderModelManager.RENDER_MODEL_TRACKER_RIGHT_HAND)
		var models := Engine.get_singleton("OpenXRRenderModelExtension")
		models.render_model_added.connect(_log_controller_models)
		models.render_model_removed.connect(_log_controller_models)
		_log_controller_models()
	print("WELD_XR_INPUT origin_offset=", right_rig.origin_offset_enabled,
		" pointer_tilt_degrees=", right_rig.pointer_tilt_degrees)


func _add_controller_models(parent: Node3D, hand: int) -> void:
	var manager := OpenXRRenderModelManager.new()
	manager.name = "ControllerModels"
	manager.tracker = hand
	manager.visible = false
	parent.add_child(manager)
	controller_models.append(manager)


func _log_controller_models(_model: RID = RID()) -> void:
	var models := Engine.get_singleton("OpenXRRenderModelExtension")
	print("WELD_XR_CONTROLLERS runtime_models=", models.is_active(),
		" count=", models.render_model_get_all().size())


func _update_controllers() -> void:
	var focused := xr_interface != null and xr_interface.is_initialized() \
		and xr_interface.get_session_state() == OpenXRInterface.SESSION_STATE_FOCUSED
	right_rig.update_tracking(focused)
	for manager in controller_models:
		manager.visible = focused


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
	_update_controllers()
	_configure_resolution()
	_update_panel_geometry()
	_sync_composition_panel()
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
	screen.global_position = camera.global_position + forward * panel_distance + Vector3.DOWN * 0.15
	screen.global_basis = Basis.looking_at(forward).rotated(forward.cross(Vector3.UP), -0.12)
	screen.show()
	_sync_composition_panel()
	placement_pending = false
	print("WELD_XR panel placed from tracked head pose")


func _configure_resolution() -> void:
	if preferences_resolved or xr_interface == null or not xr_interface.is_initialized():
		return
	if "--video-fixture" in OS.get_cmdline_user_args() or "--video-single-frame" in OS.get_cmdline_user_args():
		preferences_resolved = true
		panel.waiting_message = ""
		return
	if xr_interface.get_session_state() != OpenXRInterface.SESSION_STATE_FOCUSED:
		return
	var head := XRServer.get_tracker("head") as XRPositionalTracker
	if head == null:
		return
	var pose := head.get_pose("default")
	if pose == null or not pose.has_tracking_data:
		return
	if Time.get_ticks_msec() - preferences_wait_started >= 5000:
		panel.waiting_message = "Waiting for valid headset projection and sizing preferences"
	var eye := xr_interface.get_render_target_size()
	if not eye.is_finite() or eye.x <= 0.0 or eye.y <= 0.0:
		return
	var projections: Array[Projection] = []
	for index in range(xr_interface.get_view_count()):
		projections.append(xr_interface.get_projection_for_view(index, eye.x / eye.y, camera.near, camera.far))
	if not panel.player.configure_xr_presentation(eye, projections, panel_envelope,
		panel_distance, application_scale, panel_sampling):
		return
	preferences_resolved = true
	print("WELD_XR_SIZING eye=", eye, " scale=", application_scale,
		" sampling=", panel_sampling, " envelope=", panel_envelope, " distance=", panel_distance)
	panel.connect_source()


func _update_panel_geometry() -> void:
	var fitted: Vector2 = panel.player.xr_panel_size(panel_envelope)
	if fitted.x > 0.0 and fitted.y > 0.0 and not screen.mesh.size.is_equal_approx(fitted):
		screen.mesh.size = fitted
	var raster: Vector2i = panel.player.xr_viewport_size()
	if raster.x > 0 and raster.y > 0 and $PanelViewport.size != raster:
		$PanelViewport.size = raster
	# Explicit synchronous full-viewport rect, also used analytically by Rust
	# hit testing. No deferred Container sort can restore an old letterbox.
	panel.layout_video()


func _setup_panel_presentation() -> void:
	# Select once before video starts: replacing the viewport's XR render target
	# during playback is not a safe live A/B switch on the tested GLES runtime.
	if not use_native_panel:
		print("WELD_XR_PANEL mode=mesh (startup selection)")
		return
	composition_panel = OpenXRCompositionLayerQuad.new()
	composition_panel.name = "CompositionPanel"
	composition_panel.visible = false
	composition_panel.process_priority = 150
	composition_panel.alpha_blend = true
	composition_panel.enable_hole_punch = true
	composition_panel.sort_order = -1
	$XROrigin3D.add_child(composition_panel)
	if not composition_panel.is_natively_supported():
		composition_panel.queue_free()
		composition_panel = null
		use_native_panel = false
		push_warning("Native OpenXR quad layer unsupported; using mesh presentation")
	_apply_panel_mode()


func _apply_panel_mode() -> void:
	var native := use_native_panel and composition_panel != null
	# Keep the mesh's transform/visibility as the existing Rust hit-test target,
	# but exclude it from drawing when the compositor owns presentation.
	screen.layers = 0 if native else 1
	if composition_panel != null:
		composition_panel.visible = false
		composition_panel.layer_viewport = $PanelViewport if native else null
	_sync_composition_panel()
	print("WELD_XR_PANEL mode=", "native" if native else "mesh",
		" native_supported=", composition_panel != null,
		" eye=", xr_interface.get_render_target_size(), " panel=", $PanelViewport.size)


func _sync_composition_panel() -> void:
	if composition_panel == null:
		return
	composition_panel.global_transform = screen.global_transform
	composition_panel.quad_size = screen.mesh.size
	composition_panel.visible = use_native_panel and screen.visible


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
