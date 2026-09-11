extends Node3D

@onready var rust_bridge: WeldBridge = $WeldBridge
@onready var response: Label3D = $Label3D


func _ready() -> void:
	var panel := preload("uid://bjii61v6tg6cf").instantiate()
	panel.playback_started.connect(response.hide)
	panel.playback_stopped.connect(response.show)
	$CanvasLayer.add_child(panel)


func _on_ping_pressed() -> void:
	response.text = rust_bridge.ping()
