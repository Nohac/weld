extends Node3D
## Bounded, synthetic OpenXR regression. No decoder, network or game state.
var native_layers: Array[OpenXRCompositionLayerQuad] = []


func _ready() -> void:
	DirAccess.remove_absolute("user://xr-overlap-probe")
	var xr := XRServer.find_interface("OpenXR")
	if xr == null or not xr.is_initialized():
		push_error("WELD_OVERLAP_PROBE requires OpenXR")
		get_tree().quit(1)
		return
	get_viewport().use_xr = true
	var origin := XROrigin3D.new()
	add_child(origin)
	origin.add_child(XRCamera3D.new())
	var legacy := _canvas(Color.RED)
	var screen_copy := _canvas(Color.BLUE)
	for index in range(2):
		var layer := OpenXRCompositionLayerQuad.new()
		origin.add_child(layer)
		layer.position = Vector3(-0.6 + index * 0.6, 1.5, -2.5)
		layer.quad_size = Vector2(0.5, 0.5)
		layer.layer_viewport = legacy if index == 0 else screen_copy
		native_layers.append(layer)
	var screen_shader := Shader.new()
	screen_shader.code = "shader_type canvas_item; render_mode blend_disabled; uniform sampler2D screen_image : hint_screen_texture, filter_nearest; void fragment() { COLOR = texture(screen_image, SCREEN_UV); }"
	var screen_material := ShaderMaterial.new()
	screen_material.shader = screen_shader
	_draw(screen_copy, screen_material)
	var front := _canvas(Color.BLUE)
	var back := _canvas(Color.RED)
	var panel := WeldStereoPanel.new()
	origin.add_child(panel)
	if not panel.initialize(front, _solid(Color.BLUE)):
		push_error("WELD_OVERLAP_PROBE could not initialize native canvas")
		get_tree().quit(1)
		return
	panel.sync_panel(Transform3D(Basis.IDENTITY, Vector3(0.6, 1.5, -2.5)), Vector2(0.5, 0.5), Vector2i(128, 128), 1)
	var blend := ShaderMaterial.new()
	var blend_shader := load("res://shaders/overlap.gdshader") as Shader
	blend.shader = blend_shader
	blend.set_shader_parameter("canvas_image", front.get_texture())
	blend.set_shader_parameter("peer_0", back.get_texture())
	blend.set_shader_parameter("map_0", Projection.IDENTITY)
	blend.set_shader_parameter("count", 1)
	blend.set_shader_parameter("amount", 0.5)
	panel.output_control(false).material = blend
	panel.output_control(true).material = blend
	panel.show_panel(true)
	# Observer canvases are ordinary GPU targets, read only after their draw.
	# Native targets remain inputs only in the deliberately broken legacy case.
	var legacy_observer := _observe(legacy)
	var screen_observer := _observe(screen_copy)
	var native_output: SubViewport = panel.output_control(false).get_viewport()
	# Validate the exact native output shader independently, without sampling
	# that output through the very native-texture proxy path under test.
	var split_observer := _canvas(Color.TRANSPARENT)
	_draw(split_observer, blend)
	RenderingServer.viewport_set_parent_viewport(back.get_viewport_rid(), native_output.get_viewport_rid())
	var passed := false
	for sample in range(5):
		await get_tree().create_timer(1.0).timeout
		var old_pixel := legacy_observer.get_texture().get_image().get_pixel(64, 64)
		var screen_pixel := screen_observer.get_texture().get_image().get_pixel(64, 64)
		var split_pixel := split_observer.get_texture().get_image().get_pixel(64, 64)
		print("WELD_OVERLAP_PROBE sample=", sample, " legacy_texture=", old_pixel,
			" legacy_screen=", screen_pixel, " split=", split_pixel)
		passed = absf(split_pixel.r - 0.5) < 0.05 and absf(split_pixel.b - 0.5) < 0.05 and split_pixel.g < 0.05
	print("WELD_OVERLAP_PROBE_DONE passed=", passed)
	get_tree().quit(0 if passed else 1)


func _solid(color: Color) -> ShaderMaterial:
	var shader := Shader.new()
	shader.code = "shader_type canvas_item; uniform vec4 tint; void fragment() { COLOR = tint; }"
	var material := ShaderMaterial.new()
	material.shader = shader
	material.set_shader_parameter("tint", color)
	return material


func _canvas(color: Color) -> SubViewport:
	var viewport := SubViewport.new()
	viewport.size = Vector2i(128, 128)
	viewport.disable_3d = true
	viewport.transparent_bg = true
	viewport.render_target_update_mode = SubViewport.UPDATE_ALWAYS
	add_child(viewport)
	_draw(viewport, _solid(color))
	return viewport


func _draw(viewport: SubViewport, material: ShaderMaterial) -> void:
	var rect := ColorRect.new()
	rect.size = Vector2(128, 128)
	rect.material = material
	viewport.add_child(rect)


func _observe(source: SubViewport) -> SubViewport:
	var output := _canvas(Color.TRANSPARENT)
	var material := ShaderMaterial.new()
	var shader := load("res://shaders/native_canvas.gdshader") as Shader
	material.shader = shader
	material.set_shader_parameter("canvas_image", source.get_texture())
	_draw(output, material)
	RenderingServer.viewport_set_parent_viewport(source.get_viewport_rid(), output.get_viewport_rid())
	return output


func _exit_tree() -> void:
	for layer in native_layers:
		layer.layer_viewport = null
