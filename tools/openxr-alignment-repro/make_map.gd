extends SceneTree

func _initialize() -> void:
	var actions := OpenXRActionMap.new()
	actions.create_default_action_sets()
	for profile in actions.get_interaction_profiles():
		if profile.get_interaction_profile_path() != "/interaction_profiles/bytedance/pico4_controller":
			actions.remove_interaction_profile(profile)
	var error := ResourceSaver.save(actions, "res://pico_action_map.tres")
	print("BASELINE save Pico-only action map: ", error)
	quit(0 if error == OK else 1)
