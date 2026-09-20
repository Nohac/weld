extends Node3D
## Owns headset presentation, not the video receiver or native image lifetime.

@export_range(0.5, 2.0, 0.025) var eye_render_scale := 1.125
@export var panel_texture_filter: BaseMaterial3D.TextureFilter = BaseMaterial3D.TEXTURE_FILTER_LINEAR_WITH_MIPMAPS_ANISOTROPIC
@export_range(1.0, 3.0, 0.05) var application_scale := 1.8
@export_range(0.5, 3.0, 0.1) var panel_sampling := 2.0
@export var panel_envelope := Vector2(1.6, 1.0)
@export_range(0.2, 10.0, 0.1) var panel_distance := 2.5
@export var use_native_panel := true
@export_range(0.4, 0.9, 0.05) var secondary_window_fraction := 0.75
@export_range(0.02, 0.2, 0.01) var secondary_window_distance := 0.08

var xr_interface: OpenXRInterface
var placement_pending := true
var passthrough_active := false
var controller_models: Array[OpenXRRenderModelManager] = []
var preferences_resolved := false
var preferences_wait_started := Time.get_ticks_msec()
var composition_panel: OpenXRCompositionLayerQuad
var window_panels := {}
var window_slots := {}
var workspace_layout := WeldXrLayout.new()

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
	if "--video-fixture" in OS.get_cmdline_user_args() or "--video-single-frame" in OS.get_cmdline_user_args():
		_setup_panel_presentation()
	else:
		screen.layers = 0
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
	if not workspace_layout.recenter(camera.global_transform, panel_distance):
		return
	screen.global_transform = workspace_layout.spawn_transform(0, false)
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
	if panel.network_active:
		_update_windows()
		return
	var fitted: Vector2 = panel.player.xr_panel_size(panel_envelope)
	if fitted.x > 0.0 and fitted.y > 0.0 and not screen.mesh.size.is_equal_approx(fitted):
		screen.mesh.size = fitted
	var raster: Vector2i = panel.player.xr_viewport_size()
	if raster.x > 0 and raster.y > 0 and $PanelViewport.size != raster:
		$PanelViewport.size = raster
	# Explicit synchronous full-viewport rect, also used analytically by Rust
	# hit testing. No deferred Container sort can restore an old letterbox.
	panel.layout_video()


func _update_windows() -> void:
	var surfaces: Array[WeldSurface] = panel.player.surfaces()
	var live := {}
	var roots := {}
	var application_roots := {}
	var application_parents := {}
	for surface in surfaces:
		var key := surface.surface_id()
		live[key] = true
		if surface.kind() != 3:
			roots[surface.window_id()] = surface
		# XR placement only: a second unparented window from the same Wayland
		# client stays with its application. Do not invent protocol parentage
		# or modality; actual popup/parent geometry still takes precedence.
		# Keep the application anchor through temporary map/resize transitions;
		# a secondary window must not briefly claim a separate carousel slot.
		if surface.kind() == 0 and surface.panel_slot() < 0:
			var application := surface.application_key()
			if application_roots.has(application):
				application_parents[surface.window_id()] = application_roots[application]
			else:
				application_roots[application] = surface
		if not window_panels.has(key):
			window_panels[key] = _create_window_panel(surface)
	for key in window_panels.keys():
		if not live.has(key):
			var entry: Dictionary = window_panels[key]
			if entry.stereo != null:
				entry.stereo.queue_free()
			entry.mesh.queue_free()
			entry.viewport.queue_free()
			window_panels.erase(key)
	for key in window_slots.keys():
		if not roots.has(key):
			window_slots.erase(key)
	var placed := {}
	for _pass in range(8):
		for surface in surfaces:
			var key := surface.surface_id()
			if placed.has(key):
				continue
			var entry: Dictionary = window_panels[key]
			if not surface.is_mapped() or placement_pending:
				continue
			var logical := surface.logical_size()
			var transform: Transform3D
			var physical: Vector2
			var parent = application_parents.get(surface.window_id()) if surface.kind() == 0 else roots.get(surface.parent_id())
			if surface.panel_slot() >= 0:
				parent = null
			if (surface.kind() == 0 or surface.panel_slot() >= 0) and parent == null:
				if not window_slots.has(surface.window_id()):
					var free_slot := 0
					while free_slot in window_slots.values():
						free_slot += 1
					window_slots[surface.window_id()] = free_slot
				var slot: int = surface.panel_slot() if surface.panel_slot() >= 0 else window_slots[surface.window_id()]
				transform = workspace_layout.spawn_transform(slot, surface.panel_slot() == 1)
				physical = surface.video_player().xr_panel_size(panel_envelope)
				# Explicit companion slot stays below the main panel, not on top
				# of it. Each panel retains its own ordinary drag offset.
				if surface.panel_slot() == 1:
					physical *= 0.55
				entry.depth = 0
			else:
				if parent == null or not placed.has(parent.surface_id()):
					continue
				var parent_mesh: MeshInstance3D = window_panels[parent.surface_id()].mesh
				var factor: float = parent_mesh.mesh.size.x / parent.logical_size().x
				physical = logical * factor
				var centered := surface.kind() <= 1
				if centered:
					physical = _secondary_size(logical, parent.logical_size(), parent_mesh.mesh.size)
				transform = parent_mesh.global_transform
				var offset: Vector2 = Vector2.ZERO if centered else surface.logical_position() * factor + physical * 0.5 - parent_mesh.mesh.size * 0.5
				var forward := secondary_window_distance if centered else 0.025 + maxf(surface.stack_index(), 0) * 0.001
				transform.origin += transform.basis * Vector3(offset.x, -offset.y, forward)
				entry.depth = window_panels[parent.surface_id()].depth + 1
			transform = surface.placed_transform(transform, workspace_layout.center(), workspace_layout.orientation_pivot())
			if not entry.mesh.mesh.size.is_equal_approx(physical):
				entry.mesh.mesh.size = physical
			if not entry.mesh.global_transform.is_equal_approx(transform):
				entry.mesh.global_transform = transform
			surface.style_panel(physical, parent if surface.kind() == 3 else null)
			var raster := surface.video_player().xr_viewport_size()
			entry.draw_size = surface.layout_canvas(physical, raster)
			placed[key] = true
	# Visibility is a compositor lifecycle transition. Hiding and showing a
	# native layer each frame tears it down and registers it again in Godot.
	for key in window_panels:
		_set_entry_visible(window_panels[key], placed.has(key))
	# One order owns the complete window family and the input pick policy.
	var origin: Transform3D = $XROrigin3D.global_transform
	var left_eye := xr_interface.get_transform_for_view(0, origin).origin
	var right_eye := xr_interface.get_transform_for_view(1, origin).origin
	panel.player.sort_xr_windows(camera.global_position, left_eye, right_eye)
	for surface in surfaces:
		if not placed.has(surface.surface_id()):
			continue
		var entry: Dictionary = window_panels[surface.surface_id()]
		var transform: Transform3D = entry.mesh.global_transform
		var order: int = surface.stacking_order()
		if entry.stereo != null:
			entry.stereo.sync_panel(transform, entry.draw_size, entry.viewport.size, order)
		if entry.fallback != null:
			entry.fallback.mesh.size = entry.draw_size
			entry.fallback.material_override.render_priority = order


func _secondary_size(logical: Vector2, parent_logical: Vector2, parent_size: Vector2) -> Vector2:
	# Uniform physical scaling only: keep the client pixels/aspect and the
	# mesh used for ray input identical to the native composition layer.
	var natural := logical * (parent_size.x / parent_logical.x)
	var limit := parent_size * secondary_window_fraction
	return natural * minf(1.0, minf(limit.x / natural.x, limit.y / natural.y))


func _set_entry_visible(entry: Dictionary, shown: bool) -> void:
	if entry.mesh.visible != shown:
		entry.mesh.visible = shown
	if entry.stereo != null:
		entry.stereo.show_panel(shown)


func _create_window_panel(surface: WeldSurface) -> Dictionary:
	var viewport := SubViewport.new()
	viewport.disable_3d = true
	viewport.transparent_bg = true
	viewport.size = Vector2i(1024, 640)
	viewport.render_target_update_mode = SubViewport.UPDATE_ALWAYS
	add_child(viewport)
	var mesh := MeshInstance3D.new()
	mesh.mesh = QuadMesh.new()
	mesh.visible = false
	add_child(mesh)
	var stereo: WeldStereoPanel
	if surface.is_stereo() or use_native_panel:
		stereo = WeldStereoPanel.new()
		$XROrigin3D.add_child(stereo)
		# Mono content shares its material/decoded image, but overlap projection
		# needs a separate canvas for each eye just like packed stereo content.
		var right_material := surface.right_eye_material() if surface.is_stereo() else surface.video_material()
		if not stereo.initialize(viewport, right_material):
			push_error("Cannot present stereo without native eye layers")
			stereo.queue_free()
			stereo = null
		mesh.layers = 0
	var material := StandardMaterial3D.new()
	material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	material.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	material.texture_filter = panel_texture_filter
	material.albedo_texture = viewport.get_texture()
	var fallback: MeshInstance3D
	if stereo == null:
		fallback = MeshInstance3D.new()
		fallback.mesh = QuadMesh.new()
		material.no_depth_test = true
		fallback.material_override = material
		mesh.add_child(fallback)
	mesh.layers = 0
	var video := ColorRect.new()
	video.mouse_filter = Control.MOUSE_FILTER_IGNORE
	video.material = surface.video_material()
	video.size = viewport.size
	viewport.add_child(video)
	surface.bind_control(video)
	surface.bind_panel(mesh, stereo.right_eye_control() if stereo != null else null)
	if stereo != null:
		surface.bind_outputs(stereo.output_control(false), stereo.output_control(true))
	print("WELD_XR_WINDOW id=", surface.surface_id(), " parent=", surface.parent_id(), " native=", stereo != null)
	return {"viewport": viewport, "mesh": mesh, "video": video, "stereo": stereo, "fallback": fallback, "draw_size": Vector2.ZERO, "depth": 0}


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
