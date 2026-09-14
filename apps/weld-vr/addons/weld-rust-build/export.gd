@tool
extends EditorExportPlugin

const Build = preload("build.gd")


func _get_name() -> String:
	return "WeldRustBuild"


func _supports_platform(platform: EditorExportPlatform) -> bool:
	return platform is EditorExportPlatformAndroid


func _get_android_manifest_application_element_contents(_platform: EditorExportPlatform, _debug: bool) -> String:
	var preset := get_export_preset()
	return manifest_metadata(preset.get_custom_features(),
		preset.get_project_setting("xr/openxr/extensions/hand_tracking"))


static func manifest_metadata(custom_features: String, hand_tracking: bool) -> String:
	var pico := false
	for feature in custom_features.split(","):
		pico = pico or feature.strip_edges() == "weld_pico"
	if not pico:
		return ""
	# Pico checks capabilities before launching. This enables tracking, not
	# remote hand-gesture input, and never declares the app hands-only.
	var metadata := '<meta-data android:name="controller" android:value="1" />'
	if hand_tracking:
		metadata += '<meta-data android:name="handtracking" android:value="1" />'
	return metadata


func _export_begin(features: PackedStringArray, is_debug: bool, _path: String, _flags: int) -> void:
	if not features.has("android"):
		return
	var architectures := PackedStringArray()
	for architecture in ["armeabi-v7a", "arm64-v8a", "x86", "x86_64"]:
		if get_export_preset().get("architectures/" + architecture):
			architectures.append(architecture)
	var error := Build.android_configuration_error(is_debug, architectures)
	if error.is_empty():
		error = Build.run("android")
	if not error.is_empty():
		# Godot 4.7 export hooks cannot cancel native deployment. Keep the last
		# good library intact and put the failure in the export result dialog.
		get_export_platform().add_message(EditorExportPlatform.EXPORT_MESSAGE_ERROR,
			"Weld Rust build", error + "\nGodot may still deploy the previous library. "
			+ "This is NOT a successful Rust build; fix the error and deploy again.")
