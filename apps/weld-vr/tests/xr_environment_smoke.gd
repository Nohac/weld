extends SceneTree
## Optional discovery, visibility, animation and sky lifecycle, without assets/XR.

func _initialize() -> void:
	call_deferred("_run")

func _run() -> void:
	var viewport := SubViewport.new()
	viewport.size = Vector2i(960, 640)
	viewport.render_target_update_mode = SubViewport.UPDATE_ALWAYS
	root.add_child(viewport)
	var world := WorldEnvironment.new()
	world.environment = Environment.new()
	viewport.add_child(world)
	var camera := Camera3D.new()
	viewport.add_child(camera)
	camera.position = Vector3(0, 1.6, 0)
	camera.current = true
	var empty := WeldXrEnvironment.new()
	empty.directory = "res://tests/absent-environments"
	empty.world = world
	viewport.add_child(empty)
	empty.cycle_environment()
	if empty.get_child_count() != 0 or empty.select_environment(1):
		_fail("Absent collection must remain empty")
		return
	empty.free()
	var environments := WeldXrEnvironment.new()
	environments.directory = "res://tests/fixtures/environments"
	environments.world = world
	viewport.add_child(environments)
	var children := environments.get_children()
	if children.size() != 2 or children[0].name != "Animated" or children[1].name != "SkyScene":
		_fail("Environment discovery must have deterministic scene order")
		return
	for child in children:
		if child.visible or child.process_mode != Node.PROCESS_MODE_DISABLED:
			_fail("Environments must start hidden and paused")
			return
	var animation: AnimationPlayer = children[0].get_node("AnimationPlayer")
	for index in [1, 2, 0]:
		environments.cycle_environment()
		for number in range(children.size()):
			var child = children[number]
			if child.visible != (index == number + 1) or (child.process_mode == Node.PROCESS_MODE_DISABLED) == child.visible:
				_fail("Only the selected environment may render/process")
				return
		var previous := animation.current_animation_position
		for frame in range(8):
			await process_frame
			RenderingServer.force_draw(false)
		if index == 1 and (not animation.is_playing() or animation.current_animation_position <= previous):
			_fail("Selected environment animation must advance")
			return
		if index != 1 and not is_equal_approx(previous, animation.current_animation_position):
			_fail("Hidden environment animation must pause")
			return
		if (world.environment.sky != null) != (index == 2):
			_fail("Sky must be applied and cleared when switching")
			return
		if index > 0:
			var image := viewport.get_texture().get_image()
			var background := image.get_pixel(0, 0)
			var different := 0
			for y in range(20, 640, 20):
				for x in range(20, 960, 20):
					if not image.get_pixel(x, y).is_equal_approx(background):
						different += 1
			if different < 30:
				_fail("Selected environment has no visible content")
				return
	if environments.select_environment(99):
		_fail("Invalid environment index was accepted")
		return
	viewport.queue_free()
	await process_frame
	print("WELD_XR_ENVIRONMENT_SMOKE_OK")
	quit()

func _fail(message: String) -> void:
	push_error(message)
	quit(1)
