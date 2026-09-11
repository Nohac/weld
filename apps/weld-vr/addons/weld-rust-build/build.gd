@tool
extends RefCounted


static func android_configuration_error(is_debug: bool, architectures: PackedStringArray) -> String:
	if not is_debug:
		return "Weld Rust supports debug exports only; no release library was built."
	if architectures != PackedStringArray(["arm64-v8a"]):
		return "Weld Rust requires an ARM64-only Android preset; no library was built."
	return ""


## Cargo owns freshness checks. An empty result means the build succeeded.
static func run(target: String) -> String:
	var helper := ProjectSettings.globalize_path("res://scripts/build-gdextension")
	var output: Array = []
	print("[Weld Rust] Checking %s debug build..." % target)
	var code := OS.execute("bash", PackedStringArray([helper, target]), output, true)
	var compiler_output := "\n".join(output).strip_edges()
	if not compiler_output.is_empty():
		print(compiler_output)
	if code == 0:
		return ""
	var lines := compiler_output.split("\n")
	var tail := "\n".join(lines.slice(maxi(0, lines.size() - 12)))
	return ("Weld Rust %s build failed (exit %d). If cargo or the Android NDK is missing, "
		+ "start Godot from the shared Rust development shell. "
		+ "Full compiler output is in the Output panel.\n%s") % [target, code, tail]
