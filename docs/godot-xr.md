# Godot XR presentation

The startup scene selects `xr.tscn` when OpenXR initializes, otherwise the flat
viewer. Both use the same [Iroh receiver and native decoder](godot-hoisting.md).
XR renders each layer's mono GPU SubViewport into a native OpenXR quad
composition layer when supported, with a Godot mesh fallback. There is no
second media pipeline or CPU video readback.

The panel fits the application's clipped logical aspect within a 1.6 m by 1 m
envelope, initially 1.6 m ahead and slightly below the tracked head, tilted
back. It stays pinned rather than following head movement. Recenter places it
again. Host-keyboard capture remains separate work.

## Window layout and decoration

Independent applications occupy stable positions around the user. Related
toplevels center over their owner, capped to 75% of its physical width/height
with aspect preserved, and sit 8 cm forward. A second unparented toplevel from
the same namespaced Wayland client also uses this placement; this is a shell
grouping choice, not invented protocol parentage or modality. Menus, tooltips
and subsurfaces retain the transported relative positions and stacking.

Each native panel has the same pose and bounds as its Rust ray-input target.
The ray selects the nearest hit, while an admitted drag keeps its captured
layer through release. Creation/removal and ancestor visibility update views
without reconnecting. Native visibility changes only on actual transitions:
hiding/showing every frame recreates Godot composition layers and caused
visible flicker in the initial multi-window test.

Rounded video corners use viewport alpha; transparent corners reject new ray
hits. A small scene-rendered outline and soft shadow surround toplevels/popups
without enlarging the video or input rectangle. This shadow is a decorative
fade, not a light-cast shadow onto another panel. The active input window gets
a brighter blue border; inactive borders are muted gray. Rust reads existing
input focus, including reset/unmap/disconnect, rather than inferring focus
from scene order. Native video remains independently composed at full panel
resolution. Window moving, closing, resizing and placement persistence are
not implemented yet.

## Controller presentation

The runtime supplies controller models through Godot's standard
[OpenXR render-model support](https://docs.godotengine.org/en/4.7/tutorials/xr/openxr_render_models.html).
No Pico SDK, login, or system overlay is used. Managers are created only after
XR initialization and filtered by hand:

```text
XROrigin3D
├── XRCamera3D
├── ControllerModels (left)
└── RightControllerRig
    ├── ControllerModels (right)
    ├── Grip (raw tracked pose)
    └── Aim (raw tracked pose)
        └── PointerTilt
            └── WeldXrPointer (laser, hit marker and input projection)
```

White ambient light and a shadow-free directional light illuminate the models.
Video and pointer materials are unshaded, so lighting does not alter their
colors. XR Tools and its autoloads are no longer used.

### Offset and ergonomic tilt

| Setting | Default | Android Pico preset |
| --- | --- | --- |
| `weld/xr/controller_aim_grip_offset` | false | true |
| `weld/xr/pointer_tilt_degrees` | 5.0° downward | same |

The `weld_pico` feature enables an **empirical right-controller correction**:
the rig's position becomes raw `aim.position - grip.position`. It moves the
model and actual pointer together, retaining their relative poses and runtime
rotations. It does not overwrite either tracked pose or apply an accumulating
per-frame translation. Disable the setting to use unmodified runtime placement.
The left-hand model has no correction or input source in this slice.

Model and diagnostic-marker alignment were visually validated on Pico 4 Ultra /
Pico OS 5.15.7 with the video panel hidden. That does not validate actual input:
the final tilted ray, hit marker and clicked point still await user confirmation.
The original offset also reproduces in a standalone Godot 4.7.1 project without
Weld, and with an explicit Pico 4 interaction profile. The precise Godot-versus-
Pico cause remains unresolved; this is not a universal OpenXR calibration.
The source-only [minimal reproduction](../tools/openxr-alignment-repro/README.md)
preserves the upstream investigation without room screenshots, device dumps or
generated binaries. Related [Pico profile work](https://github.com/godotengine/godot/pull/112424)
is not a confirmed fix. A Vulkan comparison crashed inside Pico's XR runtime
and did not produce alignment evidence.

The separate tilt rotates only `PointerTilt` about local X. Positive degrees
mean downward; values clamp to ±30°, with non-finite values treated as zero.
The same transform drives the visible laser and Rust hit testing. Model
orientation is unaffected. Both settings are also exposed on the rig for
scene-level experiments. A value explicitly saved in the scene inspector takes
precedence over the project setting and its device-feature overrides; the
checked-in scene leaves these values unset.

Native tracking/model nodes run at priority 0, the XR scene updates the rig at
100, and Rust samples the pointer at 200. This is before the SceneTree flush
that publishes transforms to rendering; `frame_pre_draw` is too late for
ordinary Node3D corrections. The rig resets its translation and hides on
focus/aim loss, and also on grip loss when correction requires it. The pointer
checks ancestor visibility (not its own self-managed visibility), so this
deactivates input without preventing later recovery. Both model managers are
focus-gated; the right model additionally inherits the rig's tracking gate.

## Right-hand input

| Controller action | Application input |
| --- | --- |
| Aim at the displayed image | Pointer motion |
| Trigger | Left click/drag |
| Grip | Middle click/drag (Blender orbit) |
| Thumbstick click | Right click |
| Thumbstick up/down | Wheel up/down |

Godot generates the default OpenXR action map. Binding names are `trigger`,
`grip`, `primary_click` and `primary`, as in the
[pinned Pico profile](https://github.com/godotengine/godot/blob/a13da4feb/modules/openxr/action_map/openxr_action_map.cpp#L307).
Rust applies analog hysteresis (press at 0.75, release below 0.35), a thumbstick
deadzone of 0.35, and an eight-tick/s scroll ceiling without catch-up bursts.

One finite, front-facing ray/quad intersection within 3 m maps to **unclamped**
mono-viewport pixels. The shared displayed-image geometry handles letterboxing,
crop, input regions and capture outside the image. Missing intersections
withdraw hover while retaining the last finite release position.

Off-image presses require release before a new gesture. On-image presses
refused during native frame binding retry once per process frame while held
and still on-target. Mailbox overflow retains the shared reset/suppression
policy; retries cannot revive a suppressed hold. Tracking/focus loss, pause,
unmap, stream replacement and teardown cancel held input. Returning with a
held button or deflected stick requires release/centering first.

Godot objects and action sampling stay on the main thread. Owned events enter
the existing bounded mailbox and `ClientRuntime`, independently of video work.
Generation/epoch tokens contain no native leases. Desktop and XR cannot both
own input for the same player. Presentation geometry may be one display frame
old because publication occurs at `frame_pre_draw`; epoch checks reject stale
targets.

XR remote cursor shapes, relative pointer locking, virtual keyboard/IME,
hand-gesture input are not implemented here.

## Image quality

`eye_render_scale = 1.125` requests 2160x2160 per eye on the tested Pico's
1920x1920 recommendation, with 4x MSAA. This scales the eye target, not the
source video. Matching physical panel dimensions is not a claim of maximum
optical quality. MSAA improves geometry edges, not aliasing
already inside the video.

The preferred path is a native `OpenXRCompositionLayerQuad`: the headset
compositor samples the panel separately from Godot's eye images, avoiding the
mesh-to-eye resampling step. The same source stream looked substantially
clearer in physical Pico testing. This agrees with Godot's
[composition-layer guidance for text and UI](https://docs.godotengine.org/en/stable/tutorials/xr/openxr_composition_layers.html).
It is not direct decoder-to-swapchain presentation: the native video texture
still draws into the mono SubViewport first.

`use_native_panel` defaults to true in `xr.gd`. Native support is checked
explicitly; unsupported runtimes retain mesh presentation. Set it to false
before startup for a mesh comparison. Do not switch during playback: the live
A/B experiment produced a Godot render-target assertion and a native-image
bind failure on the tested GLES runtime. Startup-only native presentation
kept video live; the exact underlying GL failure remains unresolved. No error
checks or native image lifetime protections were weakened.

Live layers use hole punching and negative sort orders, ordered by hierarchy
and stack, so scene controllers and the pointer can draw in front. The fixture
uses sort order -1. Each pose and quad size follow the same panel-mesh
geometry Rust uses for hit testing. That mesh stays logically visible but is
excluded from rendering in native mode. Composition transforms update at
priority 150, between scene layout at 100 and pointer sampling at 200.

The fallback mesh uses Linear With Mipmaps Anisotropic filtering. Its viewport
has only a base level: GLES falls back to level-zero linear filtering while requesting
anisotropy. This is not full mipmapped minification. Source resolution, UI
scaling and bitrate remain separate readability controls.

### Application and panel sizing

Before connecting, XR waits for headset focus, tracking and valid per-eye
projections. Rust derives a stable pixel-density preference from the eye
target, projection, nominal panel distance and envelope, with sampling factor
2.0 and preferred application scale 1.8. Head movement does not trigger
application reconfiguration. Logical application size, encoded pixels and
physical panel dimensions remain distinct.

Each independent mapped root receives a bounded logical configure first, then a
preferred-scale request only after a later safe commit. A later commit is not
treated as a configure acknowledgement: both the observed root and pending
target must fit, including decoration margins, existing HiDPI scale and the
rounded-up scale used by legacy clients. The receive ceiling remains 2048 per
dimension and 1920x1080 total pixels; it is policy, not a hardware capability
claim. Applications may choose a different size, and over-limit frames still
fail admission rather than bypassing it.

Visible panel geometry and input use the same clipped logical aspect, removing
the old side borders. Viewport raster dimensions round down to 64-pixel buckets
and wait 300 ms for subsequent size changes to stabilize; the first actual
frame establishes the initial size. Layout is synchronous, without deferred
Container sorting. A resize can still have one frame of metadata/layout delay.

## Export and run on Pico

Disable **Editor Settings > Export > Android > Shutdown ADB On Exit**
(`export/android/shutdown_adb_on_exit = false`) when sharing ADB with other
tools. Godot otherwise runs `adb kill-server` on exporter shutdown. This is
an editor-local setting, not a project setting.

The **Android Pico** preset enables `weld_xr,weld_pico`, ARM64, API28 minimum,
Gradle and Internet access. The plain Phone preset does not enable the Pico
correction or Pico manifest metadata. Gradle includes the standard Khronos
OpenXR loader; generated files and native libraries remain ignored.

For Pico, the project requests OpenXR hand tracking and the export hook declares
`controller=1` plus `handtracking=1` when enabled. This combination was verified
to avoid the controller-required launch notice. It is not hands-only and does
not implement hand gestures as remote input. Turning off the hand-tracking
setting also removes that declaration. No vendor SDK or camera-feed permission
is needed for alpha-blend passthrough.

```sh
godot --headless --path apps/weld-vr --install-android-build-template \
  --export-debug 'Android Pico' "$PWD/apps/weld-vr/build/weld-vr-pico-debug.apk"
adb -s PICO_SERIAL install -r apps/weld-vr/build/weld-vr-pico-debug.apk
scripts/run-godot-hoist --serial PICO_SERIAL --app blender
```

Preserve app data when reinstalling so pairing remains stable. Gradle may
download dependencies on first use; Android Studio is not required.
`WELD_XR_INPUT` logs the resolved offset and tilt configuration once at startup.
Gradle strips the Pico library: its merged input was verified byte-for-byte
against the staged Rust library, and the APK against Gradle's stripped output.
The Phone export does not use Gradle and its library must match staging directly.

## Passthrough and lifecycle

The scene requests alpha-blend passthrough only when advertised, otherwise
opaque VR. The headset composes the camera background; Weld does not access
raw camera frames. Headset rendering uses OpenXR synchronization rather than
desktop vsync. The receiver requests the headset refresh rate through the
shared presentation API, with source-side encoder limits still enforced.

Pause/session loss stops the video producer. Automatic resume/re-admission is
not supported yet: relaunch the viewer/test source after pause. The normal UI
shows connection status until video arrives. Existing `--video-fixture` and
`--video-single-frame` diagnostics remain available through user arguments
(Android uses export `command_line/extra_args`, not ADB intent extras).

## Validation

Run the isolated `apps/weld-vr/scripts/check-gdextension --android-export` check
for scene wiring, injected tracker loss/recovery, offset/tilt invariants, export
hooks and the Phone manifest negative control. It does not validate physical
pointing. The actual Pico export is checked separately with
`apkanalyzer manifest print` for both capability entries, followed by a bounded
live Blender test: the laser, hit marker and actual selected point must agree.

For the earlier controller slice, both fresh isolated checks aborted during
editor import before reaching their tests. Independent real-project scene and manifest checks, Rust
tests, actual Phone/Pico exports and a bounded AV1 receive/presentation run are
the available evidence. The live run ended cleanly, but physical pointing and
click alignment remain pending user confirmation.

The sizing/native-layer slice passed 33 Rust tests, strict all-target Clippy,
formatting, Linux/Android debug builds, the real-project scene smoke and Pico
debug export. Desktop GPU fixture playback and cursor regression checks also
passed. Physical Pico testing confirmed the side borders disappeared, followed
by a pronounced clarity improvement with native composition. The final bounded
AV1 run presented 1517x853 pixels for 843x474 logical content (approximately
1.8 scale), reaching 2862 decoded / 2653 presented / 209 superseded frames.
Those are pipeline counters, not a measured latency or frame-rate benchmark.
The runtime reported three composition layers and 90 Hz rendering. Vendor
metadata warnings remained; this does not establish behavior on other runtimes
or long-session stability.

On 2026-09-15 the multi-window slice passed 40 Rust tests, strict Clippy,
Linux/Android debug builds, scene smoke, simultaneous native-presenter and
rounded-corner/border/shadow GPU tests, and Pico debug export. Desktop Blender
Preferences, menus/tooltips and return to the main window were manually tested.
On Pico, the user confirmed the corrected centered/smaller secondary window,
absence of the earlier composition-layer flicker, and the decoration/focus
highlight. These are bounded interactive checks, not qualification of arbitrary
window hierarchies or sustained eight-layer headset workloads.

Rust tests cover pointer policy and geometry, including downward tilt mapping
to increasing image pixel Y. The scene test checks native/rig/input priority
ordering, unchanged model transforms, non-accumulating offsets, finite/clamped
tilt and tracking-loss cleanup.

### Editor import limitation

An earlier isolated Godot 4.7.1 import crash was traced to editor documentation
shutdown: its stack reached `EditorHelp::_gen_extensions_docs` after cleanup
freed documentation state, before XR scene execution. Six unconstrained
baseline runs succeeded; this was not a measured failure rate.
The two latest import aborts have no stack and cannot be attributed to that
same cause from the available evidence.
See the pinned [deferred documentation generation](https://github.com/godotengine/godot/blob/a13da4feb/editor/doc/editor_help.cpp#L3041)
and [cleanup](https://github.com/godotengine/godot/blob/a13da4feb/editor/doc/editor_help.cpp#L3365).
Checks must still fail on that error; no timing workaround or failure suppression
is included. Core dumps are disabled to prevent oversized diagnostic files.
