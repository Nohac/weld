@tool
extends EditorPlugin

var exporter: EditorExportPlugin


class CapabilityExport extends EditorExportPlugin:
	func _get_name() -> String:
		return "DiagnosticCapability"

	func _supports_platform(platform: EditorExportPlatform) -> bool:
		return platform is EditorExportPlatformAndroid

	func _get_android_manifest_application_element_contents(_platform: EditorExportPlatform, _debug: bool) -> String:
		return '<meta-data android:name="handtracking" android:value="1" /><meta-data android:name="controller" android:value="1" />'


func _enter_tree() -> void:
	exporter = CapabilityExport.new()
	add_export_plugin(exporter)


func _exit_tree() -> void:
	remove_export_plugin(exporter)
