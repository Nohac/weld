extends Node3D

@onready var rust_bridge: WeldBridge = $WeldBridge
@onready var response: Label3D = $Label3D


func _on_ping_pressed() -> void:
	response.text = rust_bridge.ping()
