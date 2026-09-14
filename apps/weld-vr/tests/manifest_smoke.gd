extends SceneTree

const Export = preload("res://addons/weld-rust-build/export.gd")


func _initialize() -> void:
	var controller := '<meta-data android:name="controller" android:value="1" />'
	var hands := '<meta-data android:name="handtracking" android:value="1" />'
	for features in ["weld_xr,weld_pico", "weld_xr, weld_pico", " weld_pico "]:
		if Export.manifest_metadata(features, true) != controller + hands:
			push_error("Pico features must declare controller and enabled hand tracking: " + features)
			quit(1)
			return
		if Export.manifest_metadata(features, false) != controller:
			push_error("Disabled hand tracking must retain controller-only metadata: " + features)
			quit(1)
			return
	for features in ["", "weld_xr", "not_weld_pico"]:
		if not Export.manifest_metadata(features, true).is_empty():
			push_error("Non-Pico exports must not declare Pico capabilities: " + features)
			quit(1)
			return
	print("WELD_MANIFEST_SMOKE_OK")
	quit(0)
