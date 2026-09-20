extends Node3D
## Bounded reproduction of live native-window retirement, without codecs/network.


func _ready() -> void:
	var mode := FileAccess.get_file_as_string("user://xr-teardown-probe").strip_edges()
	DirAccess.remove_absolute("user://xr-teardown-probe")
	var xr := XRServer.find_interface("OpenXR")
	if xr == null or not xr.is_initialized() or mode not in ["legacy", "detach", "delayed"]:
		push_error("WELD_TEARDOWN_PROBE invalid mode or no OpenXR runtime")
		get_tree().quit(1)
		return
	get_viewport().use_xr = true
	var origin := XROrigin3D.new()
	add_child(origin)
	origin.add_child(XRCamera3D.new())
	print("WELD_TEARDOWN_PROBE_START mode=", mode)
	await get_tree().process_frame
	for cycle in range(20):
		var panels: Array[WeldStereoPanel] = []
		var sources: Array[SubViewport] = []
		for index in range(2):
			var source := SubViewport.new()
			source.size = Vector2i(256 + cycle * 2, 160)
			source.disable_3d = true
			source.transparent_bg = true
			source.render_target_update_mode = SubViewport.UPDATE_ALWAYS
			add_child(source)
			var shader := Shader.new()
			shader.code = "shader_type canvas_item; uniform vec4 tint; void fragment() { COLOR = tint; }"
			var material := ShaderMaterial.new()
			material.shader = shader
			material.set_shader_parameter("tint", Color.RED if index == 0 else Color.BLUE)
			var rect := ColorRect.new()
			rect.size = source.size
			rect.material = material
			source.add_child(rect)
			var panel := WeldStereoPanel.new()
			origin.add_child(panel)
			if not panel.initialize(source, material):
				push_error("WELD_TEARDOWN_PROBE initialization failed")
				get_tree().quit(1)
				return
			panel.sync_panel(Transform3D(Basis.IDENTITY, Vector3(index * 0.6 - 0.3, 1.4, -2.5)),
				Vector2(0.5, 0.3), source.size, 1)
			panel.show_panel(true)
			panels.append(panel)
			sources.append(source)
		# Let the XR swapchain rotate through all of its images before retirement.
		for frame in range(45):
			await get_tree().process_frame
		print("WELD_TEARDOWN_PROBE_RETIRE cycle=", cycle, " mode=", mode)
		for panel in panels:
			if mode != "legacy":
				for child in panel.get_children():
					if child is OpenXRCompositionLayerQuad:
						child.layer_viewport = null
					if child is SubViewport:
						child.render_target_update_mode = SubViewport.UPDATE_DISABLED
		if mode == "delayed":
			# Verify whether allowing render-side detachment to settle helps.
			for frame in range(3):
				await get_tree().process_frame
		for panel in panels:
			panel.queue_free()
		for source in sources:
			source.queue_free()
		for frame in range(5):
			await get_tree().process_frame
		print("WELD_TEARDOWN_PROBE_RETIRED cycle=", cycle)
	print("WELD_TEARDOWN_PROBE_DONE mode=", mode)
	get_tree().quit()
