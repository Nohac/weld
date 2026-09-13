extends Node
## Select presentation only after Godot has initialized the native XR runtime.

const FLAT_SCENE = preload("uid://d1bgda50xmko3")
const XR_SCENE = preload("res://scenes/xr.tscn")


func _ready() -> void:
	var interface := XRServer.find_interface("OpenXR")
	if interface != null and interface.is_initialized():
		print("WELD_XR OpenXR initialized; selecting spatial presentation")
		add_child(XR_SCENE.instantiate())
	else:
		if OS.has_feature("weld_xr"):
			push_warning("OpenXR did not initialize; showing the flat viewer instead")
		print("WELD_XR selecting flat presentation")
		add_child(FLAT_SCENE.instantiate())
