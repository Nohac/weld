@tool
extends EditorExportPlugin

const Build = preload("build.gd")


func _get_name() -> String:
	return "WeldRustBuild"


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
