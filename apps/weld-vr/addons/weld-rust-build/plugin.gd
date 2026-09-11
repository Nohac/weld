@tool
extends EditorPlugin

const Build = preload("build.gd")
const RustExport = preload("export.gd")

var _export_plugin: EditorExportPlugin


func _enter_tree() -> void:
	_export_plugin = RustExport.new()
	add_export_plugin(_export_plugin)


func _exit_tree() -> void:
	remove_export_plugin(_export_plugin)
	_export_plugin = null


func _build() -> bool:
	var error := Build.run("desktop")
	if not error.is_empty():
		push_error(error)
	return error.is_empty()
