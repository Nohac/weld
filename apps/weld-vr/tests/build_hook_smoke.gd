extends SceneTree

const Build = preload("res://addons/weld-rust-build/build.gd")


func _initialize() -> void:
	call_deferred("_run")


func _fail(message: String) -> void:
	push_error(message)
	quit(1)


func _run() -> void:
	if not Build.android_configuration_error(true, PackedStringArray(["arm64-v8a"])).is_empty():
		_fail("ARM64 debug should be supported")
		return
	for architectures in [[], ["x86_64"], ["arm64-v8a", "x86_64"], ["armeabi-v7a"]]:
		if Build.android_configuration_error(true, PackedStringArray(architectures)).is_empty():
			_fail("Unsupported Android architecture accepted")
			return
	if Build.android_configuration_error(false, PackedStringArray(["arm64-v8a"])).is_empty():
		_fail("Release export should report unsupported configuration")
		return
	if not Build.run("desktop").is_empty() or not Build.run("android").is_empty():
		_fail("Successful fixture build rejected")
		return
	var marker := FileAccess.open("res://scripts/fail-build", FileAccess.WRITE)
	if marker == null:
		_fail("Could not enable failure fixture")
		return
	marker.close()
	var desktop_error := Build.run("desktop")
	var error := Build.run("android")
	DirAccess.remove_absolute(ProjectSettings.globalize_path("res://scripts/fail-build"))
	if not desktop_error.contains("exit 23") or not error.contains("exit 23") or not error.contains("WELD_EXPECTED_COMPILER_FAILURE"):
		_fail("Compiler failure or diagnostic was lost")
		return
	if FileAccess.get_file_as_string("res://scripts/build-modes") != "desktop\nandroid\ndesktop\nandroid\n":
		_fail("Build hooks selected the wrong target")
		return
	print("WELD_BUILD_HOOK_SMOKE_OK")
	quit(0)
