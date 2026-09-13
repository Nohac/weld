# Godot XR presentation

The startup scene selects `xr.tscn` when Godot has initialized OpenXR, otherwise
the flat viewer. Both show the same video-only panel and use the existing
[Iroh receiver and native decoder](godot-hoisting.md). There is no second media
pipeline or CPU video readback: XR samples a mono 1600x1000 GPU SubViewport on
a quad in both eyes. This panel raster does not change source window size or
the receiver's codec admission limits.

The XR scene contains an `XROrigin3D` and `XRCamera3D`. After the first valid
head pose, it places a 1.6 m wide panel 1.6 m ahead, slightly below eye level
and tilted back. The panel remains pinned, not attached to head motion.
The runtime's recenter signal places it again. Controller input, multi-window
layout, application UI scaling and source keyboard capture are not implemented
by this scene.

The scene's `eye_render_scale` is currently **1.125** for the Pico quality trial,
applied before the first XR viewport draw. It scales the runtime-recommended eye
target, not the source video or the panel SubViewport. On the measured 1920x1920
recommendation this requests 2160x2160 per eye (about 27% more eye pixels).
Set it back to 1.0 in the XR scene/script to compare the runtime default. This
does not claim that matching physical panel dimensions is an optical-quality
maximum; lens distortion, filtering and source resolution remain separate.
The XR viewport also enables **4x MSAA** for geometric edge antialiasing. The
flat viewer and mono panel SubViewport are unchanged. MSAA does not remove
aliasing already inside the video texture.

For the quick texture-filter comparison, the XR script exposes
`panel_texture_filter` and currently selects **Linear With Mipmaps Anisotropic**
on the 3D panel material. The viewport has only its base level: this option does
not generate mipmaps. Godot's GLES sampler falls back to level-zero linear
filtering while requesting anisotropy where supported. This can help oblique
sampling, but is not full mipmapped minification. Choose **Linear** to compare
against base-level bilinear filtering. The external decoder texture already uses
linear filtering and is unchanged; no readback, new texture allocation pipeline
or source-resolution change is part of this trial.

## Export and run on Pico

Select **Android Pico** for editor device deployment. That preset has:

- OpenXR mode and immersive fullscreen enabled;
- `weld_xr`, which enables OpenXR at engine startup without requiring a runtime
  for desktop or the separate **Android Phone** preset;
- ARM64, API28 minimum and **Gradle build enabled**;
- the existing debug Rust extension/build hook and Internet permission.

Godot XR Tools supplies scene/script helpers, **not the native OpenXR loader**.
With the tested Godot 4.7.1 template, the non-Gradle export omitted
`libopenxr_loader.so`. Gradle automatically adds
`org.khronos.openxr:openxr_loader_for_android:1.1.54`; the resulting APK contains
the loader. No Pico SDK, vendor plugin, developer login or camera-feed permission
is added. Generated Gradle files and binaries remain ignored.

From the repository root:

```sh
godot --headless --path apps/weld-vr --install-android-build-template \
  --export-debug 'Android Pico' "$PWD/apps/weld-vr/build/weld-vr-pico-debug.apk"
adb -s PICO_SERIAL install -r apps/weld-vr/build/weld-vr-pico-debug.apk
scripts/run-godot-hoist --serial PICO_SERIAL --app blender
```

The first command installs the matching Godot Android build template if needed;
Gradle may download dependencies on the first export. Android Studio is not
required. Preserve app data when updating so the saved identity stays stable.
For a desktop OpenXR runtime, launch Godot with `--xr-mode on`; ordinary desktop
runs stay flat. XR selection is based on runtime initialization, not a headset
model-name list. A failed XR initialization logs a warning and falls back flat.

## Passthrough and lifecycle

The scene requests `XR_ENV_BLEND_MODE_ALPHA_BLEND` only when advertised and
makes the main viewport background transparent only after the runtime accepts
the request. Otherwise it requests opaque VR. Passthrough is composed by the
headset runtime, not a raw camera feed accessible to Weld. See Godot's
[XRInterface blend modes](https://docs.godotengine.org/en/4.7/classes/class_xrinterface.html)
and [Android XR export guidance](https://docs.godotengine.org/en/4.7/tutorials/xr/deploying_to_android.html).

Headset rendering uses OpenXR's synchronization rather than desktop vsync.
The receiver requests the reported headset refresh rate through the existing
presentation API; the source still clamps it to encoder limits.

Stop/session loss and application pause stop the native video producer.
Automatic resume/re-admission remains unsupported: relaunch the viewer and its
test source after pause. The normal UI contains no test buttons; run with the
existing `--video-fixture` or `--video-single-frame` user argument for diagnostics
(on Android, use export `command_line/extra_args`, not ADB intent extras).
Connection status is shown until a native image is available. Remote input and
Blender dialogs/additional streams remain outside this presentation slice.

## Validation: 2026-09-13

On the connected Pico A9210, the corrected APK created OpenXR 1.1.54 on
`Pico XRRuntime() 119.0.65537`, automatically selected XR, reported
`passthrough=true blend_modes=[0, 2]`, and placed the panel from a tracked head
pose. These logs confirm runtime acceptance, not visual passthrough quality.
The live Blender test subsequently connected over Iroh and reported 7 decoded,
3 presented and 4 superseded startup frames before the scene became static.
The Pico preset needed its own Internet permission enabled. The launcher now
waits for the new Android process as well as the persisted public identity file;
an old identity file alone does not mean the new activity has started.
Godot reported that rendering features disabled subsampled-image foveation;
this is not implementation of Weld's proposed gaze-driven streaming.

A subsequent on-device `WELD_XR_RENDER` diagnostic, sampled after the first draw,
reported **1920x1920 per eye**, two views, XR size multiplier 1.0, an actual main
viewport texture of 1920x1920, viewport 3D scale 1.0, MSAA disabled (0), and a
1600x1000 panel texture. This is the runtime-recommended eye target at default
scale, not the headset's physical 2160x2160 panel resolution or its maximum
supported render size. The reported refresh at this startup sample was 73 Hz;
it is not a sustained refresh measurement. No quality settings were changed.
The diagnostic logs once after startup/session-begin drawing and reads sizes
only, without GPU pixel readback. Evidence: `godot-hoist-urijud6m/xr-render.log`
under `target/validation`.

The 1.125-scale comparison then confirmed both the OpenXR eye target and actual
viewport texture at **2160x2160**, with two views, 3D scale 1.0, MSAA still off
and the panel still 1600x1000. AV1 reception/presentation continued. Evidence:
`godot-hoist-hzbypnbg/xr-render.log`. This is startup/presentation validation,
not a sustained performance or subjective sharpness result.

The following 4x-MSAA trial reported `msaa_3d=2` (`Viewport.MSAA_4X`) with
2160x2160 eye targets and the same 1600x1000 panel. The Blender AV1 stream still
decoded and presented. Evidence: `godot-hoist-dwzgkjmw/xr-render.log`.

The user confirmed clean panel edges with MSAA, and a modest improvement with
the anisotropic-filter trial (`panel_filter=5`, `godot-hoist-cx1fo3n9`). Text and
UI clarity remain limited; source resolution/UI scale and bitrate tuning are
deferred. Pico's controller-required launch prompt also remains unresolved.

The isolated integration checks pass for flat fallback, the XR camera and mono
viewport/material connection, extension state and editor build hooks.
The desktop native-video check stopped receiving draw callbacks after its first
draw; the unchanged committed scene reproduced the same failure in an isolated
checkout. Desktop playback is therefore not requalified by that test run.
