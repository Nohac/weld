# Weld VR

Godot shell for Weld's VR client, initially developed on an Android phone.
Keep the Godot project and future Rust GDExtension together here; reuse existing
Weld crates rather than duplicating protocol, transport or media logic.

The project uses the Mobile renderer. Godot XR Tools is enabled, including its
user-settings and rumble-manager autoloads. OpenXR startup is not enabled yet:
phone-first setup does not require a headset runtime. Headset support must use
standard OpenXR, without a Pico SDK, Pico XR plugin or developer-account login.

## Godot XR Tools

Vendored without local changes under `addons/godot-xr-tools/`:

- Release: [4.5.1](https://github.com/GodotVR/godot-xr-tools/releases/tag/4.5.1),
  the latest stable release checked on 2026-09-11. This is the toolkit version,
  not the Godot engine version (4.7.1 in the Android development shell).
- Archive: [godot-xr-tools.zip](https://github.com/GodotVR/godot-xr-tools/releases/download/4.5.1/godot-xr-tools.zip).
- SHA-256: `f60d15e6b1bc4e544947691cb8de73c483dfe9e9a4c4dad92511e0d8f575dcae`.
- License: [MIT](addons/godot-xr-tools/LICENSE), with upstream asset notices
  retained in the add-on.

Only the add-on directory is installed, not the demo project or vendor plugins.
For updates, use an explicit upstream stable release, verify its archive digest,
and review the replacement before importing it with Godot. Keep upstream files
unchanged; put Weld behavior outside the add-on.

Open this directory from the Android development shell. If the editor was open
during installation, reload the project so it discovers the add-on's classes and
autoloads. See the shared shell's README for SDK/NDK and export-template setup.

No runnable shell scene, Rust bridge, Android export preset or headset validation
is included yet. Editor import is not validation of Android or OpenXR behavior.
An isolated copy passed `godot --headless --path <project> --import` with Godot
4.7.1 on 2026-09-11, with no errors or warnings in the Godot log.
