# Pico runtime-model alignment reproduction

Observed 2026-09-14 with Godot 4.7.1 Nix build a13da4feb,
Pico 4 Ultra, Pico OS 5.15.7, OpenXR runtime reported as 119.0.65537.

This is a standalone Godot project: no Weld native extension, FFmpeg,
network connection, video texture, input forwarding, controller model assets,
or hand-written model offsets. Godot obtains the controller model from the
runtime through OpenXRRenderModelManager under an identity XROrigin3D.
The camera is a direct child of that origin.

The application requests alpha-blend passthrough and lights the runtime model.
With the tracked controller resting on a table, its rendered model is visibly
displaced from the physical controller in the camera background. This also
occurred with the same minimal scene restricted to the Pico 4 interaction
profile; the selected profile was verified at runtime. The default action map
also reproduces it. Testing default 1.0 eye scale in the earlier Weld diagnostic
did not remove the displacement.

This localizes the reproduction outside Weld but does not yet distinguish
Godot's import/rendering from Pico runtime model/pose/passthrough behavior.
Do not treat differing grip and render-model axes alone as a bug.

## Build

Configure Godot's Android SDK, JDK and debug signing settings. Disable the
editor-local export/android/shutdown_adb_on_exit setting to avoid killing a
shared ADB server. Then:

    godot --headless --path . --install-android-build-template --export-debug 'Android Pico' baseline.apk

The package name is com.example.weldvr, so this replaces that installed app.
Use adb install -r to preserve its data and reinstall the original APK afterward.
The temporary export plugin declares handtracking=1 and controller=1 and the
project enables OpenXR hand tracking, avoiding Pico's controller-required
notice on the tested device. No hand interaction is implemented.

For the explicit-profile comparison, generate pico_action_map.tres using:

    godot --headless --xr-mode off --path . --script res://make_map.gd

Then set xr/openxr/default_action_map to res://pico_action_map.tres.

Changing the rendering method to Mobile/Vulkan did not yield an alignment
comparison: the app crashed with top native frames in Pico's XRRuntime.apk.
Do not count that failed run as evidence of either correct or incorrect
Vulkan alignment. The working reproduction uses Compatibility/OpenGL.

Screenshots and device dumps are intentionally not included: they contain
the user's room. No upstream issue has been filed by this diagnostic.
