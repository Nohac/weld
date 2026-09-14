extends Node3D
## Presentation correction shared by the right model and pointer, not raw poses.

@export var origin_offset_enabled: bool = ProjectSettings.get_setting_with_override("weld/xr/controller_aim_grip_offset")
@export_range(-30.0, 30.0, 0.5) var pointer_tilt_degrees: float = ProjectSettings.get_setting_with_override("weld/xr/pointer_tilt_degrees")

@onready var grip: XRController3D = $Grip
@onready var aim: XRController3D = $Aim
@onready var pointer_tilt: Node3D = $Aim/PointerTilt


func update_tracking(focused: bool) -> void:
	var degrees := clampf(pointer_tilt_degrees, -30.0, 30.0) if is_finite(pointer_tilt_degrees) else 0.0
	pointer_tilt.rotation = Vector3(-deg_to_rad(degrees), 0.0, 0.0)
	# Always derive from local tracked poses, never last frame's adjusted globals.
	position = Vector3.ZERO
	var valid := focused and aim.get_has_tracking_data() and aim.transform.is_finite()
	if origin_offset_enabled:
		valid = valid and grip.get_has_tracking_data() and grip.transform.is_finite()
		if valid:
			var offset := aim.position - grip.position
			valid = offset.is_finite()
			if valid:
				position = offset
	visible = valid
