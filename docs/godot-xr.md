# Godot XR presentation

The startup scene selects `xr.tscn` when OpenXR initializes, otherwise the flat
viewer. Both use the same [Iroh receiver and native decoder](godot-hoisting.md).
XR renders each layer's mono GPU SubViewport into a native OpenXR quad
composition layer when supported, with a Godot mesh fallback. There is no
second media pipeline or CPU video readback.

The panel fits the application's clipped logical aspect within a 1.6 m by 1 m
envelope. Independent windows spawn at a 2.5 m radius around the initial head
position, slightly below eye level. The workspace stays pinned rather than
following head movement; recenter establishes a new anchor. Host-keyboard
capture remains separate work.

## Window layout and decoration

Independent applications occupy stable positions around the user. Related
toplevels center over their owner, capped to 75% of its physical width/height
with aspect preserved, and sit 8 cm forward. A second unparented toplevel from
the same namespaced Wayland client also uses this placement; this is a shell
grouping choice, not invented protocol parentage or modality. Menus, tooltips
and subsurfaces retain the transported relative positions and stacking.

Rust owns spherical spawn and drag placement for both mono and stereo windows.
Position and facing use separate anchors: windows turn horizontally toward a
virtual point 0.5 m behind the workspace's initial head pose, making the curve
gentler without pushing the actual windows farther away. Vertical pitch instead
uses elevation from the pinned head position. A 6-degree backward bias at eye
level fades smoothly to zero by 15 degrees above eye level, so raised windows
tilt down toward the user. Pitch is bounded to ±85 degrees without roll. Both
anchors change only on placement/recenter, not head lean.
Azahar's explicit companion slot starts below its primary at the same radius.

Native panels include transparent margins for shell chrome; the Rust hit plane
and content Control retain the original application bounds. Windows are ordered
back-to-front by their root distance from the viewer, with a 1.5 cm hysteresis
threshold. Popups and content layers stay grouped; related toplevels can move
independently. Both stereo eyes and pointer picking use the same order, so
intersecting windows behave as whole cards rather than cutting through one
another. An admitted drag keeps its captured layer through release.
One adjacent pair of window families can crossfade through a 3 cm depth band.
Only their overlapping image area blends, using eye-specific projected window
coordinates; video and decorations fade together without revealing the room
through opaque content. Picking follows the visually dominant family while an
existing drag stays captured. Multiple simultaneous crossings retain ordinary
whole-window ordering. This is a center-distance transition, not a depth-buffer
intersection or angle-dependent fade.
Creation/removal and ancestor visibility update views
without reconnecting. Native visibility changes only on actual transitions:
hiding/showing every frame recreates Godot composition layers and caused
visible flicker in the initial multi-window test.

Rounded video corners use viewport alpha; transparent corners reject new ray
hits. A small canvas-rendered outline and soft shadow surround toplevels/popups
without enlarging the video or input rectangle. This shadow is a decorative
fade, not a light-cast shadow onto another panel. The active input window gets
a brighter blue border; inactive borders are muted gray. Rust reads existing
input focus, including reset/unmap/disconnect, rather than inferring focus
from scene order. Native video remains independently composed at full panel
resolution.

Client-owned content subsurfaces share the owning window's rounded clip rather
than bypassing it or gaining independent rounded edges. The same clip applies
to both stereo eyes and ray hit testing. Borders, shadows and controls are drawn
into the window's native canvas, not independently depth-tested scene geometry.
Padded presentation canvases are bounded to 4096 pixels per dimension; decoded
image limits and application input extents are unchanged. Mesh fallback uses
the same painter order without inter-window depth testing.

Only the pointed-at window reveals its slim close/drag strip, above or below
the nearer horizontal edge. Opacity rises with proximity and reaches full at
the edge; controls stay visible while hovered or dragged. The close button is
on the left and activates on A release over it. A on the handle or the side
grip over the window starts a captured shell drag. Moving the controller changes
the window position, including push/pull depth; facing follows the pinned
orientation pivot rather than wrist rotation. There is no stick depth mapping.
Placement radius is bounded to 0.6–5 m and elevation to ±85 degrees; window
physical size stays unchanged. Parent-relative placement keeps attached
windows/popups following their owner. Shell gestures do not
forward clicks to the application underneath; recenter or tracking/focus loss cancels
the gesture and requires held controls to be released before reuse.
Closing sends the existing surface close request, not a process kill.
Rounded corner handles float 2 cm outside the window. A-drag previews a centered
resize without changing its pose or reconfiguring the application on every
motion; release sends one bounded logical-size request. Until matching content
arrives, the old image is aspect-fitted rather than stretched. Packed stereo
resizes preserve aspect. Tracking/focus loss cancels the drag.
The strip's minus/plus controls change application UI scale in 20-percentage-point
steps (100–300%), not physical window size. Resolution requests retain the
existing pixel/dimension limits. Placement persistence is not implemented yet.

### Optional local environments

Place self-contained Godot scene wrappers (`.tscn` or `.scn`, `Node3D` roots) at
the top level of `apps/weld-vr/environments/`, with their models/textures beneath
that folder. The entire folder is Git-ignored; no downloaded scenery is required
by a clean checkout. The current `all_resources` export presets include local
scenery when present. ResourceLoader discovers scenes once at startup, including
export-remapped resources, in filename order. No generated manifest is needed.
Use numeric filename prefixes to choose the cycle order.

Left grip in shell mode cycles these scenes and passthrough. With no scenes,
cycling does nothing; gamepad mode retains its grip mapping. Hidden scenes stop
processing. Scenery is pinned at recenter, not attached to ordinary head motion.
Optional root metadata `background_color` (Color), `sky` (Sky), and `animation`
(StringName naming an AnimationPlayer clip) select the background and looping
animation. An absent folder is normal; invalid scenes warn and are skipped.
Scenery consumes headset GPU budget independently of the streamed video.

The 2026-09-19 Pico Azahar comparison established the 2.5 m spawn distance,
rear orientation pivot and softened pitch through user testing. The slice
passed 86 Rust tests, strict Clippy, scene and GPU shader checks, and Android
build/export. The shader checks cover both stereo eyes and owner-relative
subsurface clipping. This is presentation validation, not a new codec or
performance qualification.

On 2026-09-20, the user accepted the separate head-centered vertical curvature
and closer 0.5 m horizontal pivot in Pico run `godot-hoist-h8fk_5xl`. The six
focused placement tests passed, including pitch independence from the yaw
pivot, fade endpoints, unchanged positions and the stronger horizontal turn.

Whole-window stacking was accepted in Pico run `godot-hoist-pis6cf62` on
2026-09-20. The slice passed 91 Rust tests, strict Clippy, scene and real GPU
shader checks, and Android build/export. Coverage includes ordering changes,
near-equal depth stability, family grouping, input order, and canvas padding.

The accepted overlap fade uses ordinary sampleable source canvases and separate
native OpenXR output canvases. All sources render before the outputs that sample
them, including across windows. Native swapchain viewports are not shader inputs:
on Pico, that path produced black or incorrect cross-window samples. Godot's
[GLES proxy-remapping implementation at a13da4feb](https://github.com/godotengine/godot/blob/a13da4feb/drivers/gles3/storage/texture_storage.cpp#L1349)
retains a thread-local proxy list initialized from the first texture; this is
consistent with the observed cross-texture failure, not a confirmed upstream
diagnosis. The isolated Pico color probe `xr-overlap-probe-c6yqxivc` verified the
separate-canvas blend. The one-shot `user://xr-overlap-probe` marker runs that
bounded diagnostic instead of connecting to a source. Rust policy tests, scene
and GPU shader checks cover grouping, blend math, projection, input order and
same-frame sampling. This adds GPU canvas work; it is not a performance claim.

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
| A | Left click/drag, or activate shell controls |
| B | Right click |
| Index trigger | Middle click/drag (Blender orbit) |
| Side grip | Move the pointed-at window |
| Thumbstick up/down | Continuous variable-speed scrolling |

Godot generates the default OpenXR action map. Binding names are `ax_button`,
`by_button`, `trigger`, `grip` and `primary`, as in the
[pinned Pico profile](https://github.com/godotengine/godot/blob/a13da4feb/modules/openxr/action_map/openxr_action_map.cpp#L307).
Rust applies analog hysteresis (press at 0.75, release below 0.35). Scrolling
uses a 0.2 dead zone and a quadratic deflection curve up to 1200 logical units/s,
with explicit continuous-axis stop, no synthetic wheel ticks and no catch-up
after a stall. Equal deflection travels the same distance at 60/72/90/120 Hz.

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

Live layers use positive sort orders above the main projection without hole
punching. Rectangular hole punches removed scenery beneath transparent window
margins, turning soft shadows black. Hierarchy/stack order is unchanged; the
fixture uses sort order 1. The laser and hit marker are also drawn in each eye's
native window canvas where they lie in front of the window plane, preserving
canvas alpha. The ordinary 3D laser remains in the scene. This adds no XR layers
or render targets; controller meshes themselves remain in the main projection.
Each pose and quad size follow the same panel-mesh
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

The current checkpoint uses Godot 4.8-dev6 and matching Android templates.
See [engine issues and workarounds](godot-engine-issues.md) for preview selection,
the Nix SDK compatibility view, native teardown evidence and remaining errors.

From the shared development shell, `scripts/run-godot-xr` checks native build
freshness, exports the Pico APK when needed, installs it and launches Blender
for three minutes. Launching does not run tests, formatting, Clippy or scene
probes. Run `scripts/check-godot-xr` separately for those checks; it builds the
desktop extension and runs Rust and Godot checks in parallel without deployment.
Cargo checks share the native build's explicit desktop target. Build-tool
selection stays stable across Godot's Android export hook when Godot adds Java.
The extension build no longer watches the entire PATH: cc/pkg-config track
compiler and library settings, and the APK cache fingerprints resolved tool
paths/versions rather than unrelated shell PATH entries. Successful preparation
is cached against source inputs, staged libraries, toolchain, relevant environment
and APK contents; `--recheck` forces export, not validation.
The 2026-09-20 reuse check `xr-build-reuse-ea7tlh0w` changed only an unrelated
PATH prefix and reused both native targets and the APK in 1.4 seconds. Eleven
script tests passed, including invalidation for changed tools, flags and content.
Build/export failures prevent installation. Use `--app foot`, `--seconds`,
`--serial`, or `--adb` as needed. Preparation logs stay under
`target/validation/godot-xr-*`; standalone checks use `godot-xr-check-*`.

AV1 remains the default codec; `--codec h264` selects the comparison path in
both `run-godot-xr` and `run-godot-hoist`. Diagnostic plots label the codec from
encoder logs, falling back to explicitly unconfirmed requested run metadata.
The shared target defaults to 16 Mbps; `--bitrate-mbps 8|16|24` selects the
test's total encoder target, not a bandwidth cap or a per-window guarantee.
Per-stream codec limits still apply. This does not change Weld's general default.
Starting another helper on the same device requests a graceful stop of the
recorded prior owner and waits for its lock to release after cleanup (up to
45 seconds). PID generation is validated and signaling uses a pidfd. Other
devices and the shared ADB server are untouched. A pre-upgrade helper lacking
owner metadata may need to be stopped manually once.
The shared preparation lock also spans the demo, preventing a second helper's
build/check workload from interfering with timing measurements on another device.

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
scripts/run-godot --headless --path apps/weld-vr --install-android-build-template \
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
It rechecks the reported rate every 250 ms and forwards only changes through
the coordinator's latest-value mailbox. Existing and newly discovered windows
use that preference without restarting the connection. Invalid samples retain
the last preference; 60 Hz is only the initial fallback. `WELD_PRESENTER_RATE`
logs screen and OpenXR readings alongside the selected milliHz, while the source
logs each opened encoder generation under `weld_media_diag`. A reported rate is
not a measurement of delivered video FPS or a guarantee of codec throughput.

Pause/session loss stops the video producer. Automatic resume/re-admission is
not supported yet: relaunch the viewer/test source after pause. The normal UI
shows connection status until video arrives. Existing `--video-fixture` and
`--video-single-frame` diagnostics remain available through user arguments
(Android uses export `command_line/extra_args`, not ADB intent extras).

## Validation

### Frame pacing checkpoint and diagnostics

The normal desktop/XR receiver now retains at most two complete window
snapshots to absorb short arrival bursts. Layers advance together; retained
pixels survive snapshot replacement until consumed or genuinely discarded.
The first or sole remaining frame does not wait for prefill. With a newer
snapshot available, an older snapshot expires after two presenter intervals,
capped at 34 ms. Resize/unmap/layout changes discard obsolete queued layouts.
This is bounded smoothing, not adaptive playback timing or an ACK mechanism;
input scheduling, native leases and GPU fences are unchanged.

Generate and open an interactive HTML/uPlot report for the latest run:

```sh
scripts/plot-godot-hoist
scripts/plot-godot-hoist target/validation/godot-hoist-RUN --no-open
```

The report embeds measurements and loads pinned uPlot 1.6.32 assets from a CDN
(Internet access or cached assets required). Frame outcomes and network RTT
are adjacent, with synchronized cursors and zoom. Dashed whole-run averages,
visible-range averages, measured counter totals, and an optional whole-run
Y-axis lock provide context while zoomed. Rate averages are duration-weighted
over measured intervals; sample/maxima averages are labeled separately. Gaps
and unknown initial counter baselines are excluded. Fixed-width legend columns
and a 70px minimum legend height keep hover updates stable.

The report separates source coalescing, decode/queue timing, presentation
replacement, stale-snapshot expiry, layout/lifecycle discards, blocked render
and fence checks, media-write cancellation, QUIC packets declared lost, and
runtime-reported late XR frames. These are different units and must not be
added into one dropped-frame total. Imports are not physical scanouts, and
clock-local durations overlap; the charts do not establish one-way network
latency. Legacy logs retain an unknown-reason superseded category and visibly
unavailable stages rather than invented zeros.

`run-godot-hoist` enables media and network summaries for both timed and unlimited
runs unless explicitly overridden in `RUST_LOG`. Duration no longer changes
source logging. The report includes source commits/coalescing, encode batch
timing, pending work, encoded frames/payload bitrate, receiver ingress/decode
throughput and queues. Bursty presentation commits are measured after decoding,
not at application production. Missing source/network samples produce explicit
warnings; absent packet-loss data is not a zero-loss measurement.
Godot's application-owned tracing bridge forwards
bounded summaries to the main-thread logger, splitting long Android messages
into numbered chunks to avoid logger truncation. Full-run filtered logcat is
captured in `viewer.log`, capped at 16 MiB, with explicit gap markers. Generated
reports, logs, clips, APKs and native libraries remain untracked.

The independent local-decoder stress scene uses the existing native provider
and texture presentation, without Iroh or the live shared decode pool:

```sh
scripts/run-godot-video-stress --case mixed-latest --case mixed-smooth --seconds 25
scripts/run-godot-video-stress --case mixed-heavy --seconds 25
```

Use `--adb`/`--serial` to select the shared device connection, `--desktop` for
Linux EGL, or `--skip-build` only when the matching APK is already installed.
The mixed case feeds one 1440p90 stream at an 8 Mbps encoder target, two
1080p60 streams at 3 Mbps each, and five 360p30 streams at 0.5 Mbps each.
The heavy case raises both secondary streams to 1440p90. Targets are not
measured network usage. Short software-generated AV1 clips loop for a bounded
duration; the test does not create an unbounded video recording.

On 2026-09-16, a 60-second mixed smoothing run measured about 89.96, 60 and
30 texture updates/s respectively, with 8.0 ms mean reported GPU time and no
reported late XR frames. Three 1440p90 streams plus five small streams reached
about 87 updates/s at 11.4 ms GPU time, despite decoding near 90 fps. In one
controlled live Blender comparison, the measured active single-window tail
improved from roughly 66 to 87 texture updates/s. Local A/B tests measured
roughly 10 ms more decoded-frame selection age with smoothing. These are
bounded observations, not hardware capacity guarantees or end-to-end latency.
These measurements used the extension's standalone, unoptimized Cargo dev
profile. It does not inherit the main Weld workspace's opt-level 1 / optimized
dependency profile. The extension and its Rust dependencies now use dev
opt-level 3, retaining debug assertions and limited debug information for
`weld-vr`, but no dependency debug information. Linux/Android builds and physical
Pico playback passed; optimization did not eliminate frame replacements.

For an opt-in half-rate live comparison, keeping the same bitrate:

```sh
scripts/run-godot-xr --half-rate --bitrate-mbps 16
# Full-rate comparison:
scripts/run-godot-xr --bitrate-mbps 16
```

The separate Android decoder experiment is selected with
`scripts/run-godot-xr --decoder-low-latency --bitrate-mbps 16`. Omit the flag
for baseline, or combine it with `--half-rate` for a matched half-rate comparison.
See [receiver efficiency](receiver-efficiency.md) for the audit, FFmpeg patch,
ownership constraints and limits of what the request establishes.

The lower-level `run-godot-hoist` launcher also accepts `--half-rate` when the
current APK is already installed. It writes a private, one-shot diagnostic
marker consumed by the receiver at session startup. Only the outgoing source
cadence preference is halved; XR refresh, input processing, queue capacity and
the local frame-age limit remain unchanged. New windows, refresh changes and
connection retries keep that session's policy. An ordinary launch defaults to
full rate.

On 2026-09-17, logs confirmed 45 fps AV1 encoder configuration with the Pico
still presenting at 90 Hz. The user reported substantially fewer replacements
than at full rate, but a late period of lag spikes coinciding with increased
network RTT. This is an observed correlation, not a confirmed cause or a
controlled latency comparison. Catch-up scheduling remains deferred; this
experiment does not add buffering or change frame-discard policy.

On 2026-09-18, cooperative decoder polling and receiver-owned inventory passed
86 Rust tests, Linux/Android Clippy, desktop native playback, and a physical
half-rate source-reconnection test. One decoder-only comparison reduced average
worker residence from about 6.48 to 5.72 ms, but replacements did not consistently
improve across automated runs, and network loss differed between runs. The user
reported slightly smoother full-rate playback and possibly fewer replacements
at both rates. These observations do not establish a controlled performance gain.

A full-rate Preferences-close test also produced a native Godot GL-thread
SIGSEGV. Earlier baseline runs reproduced texture-cleanup errors, but not that
crash. Later 4.7.1 crash diagnosis and bounded 4.8-dev6 validation are recorded in
[engine issues](godot-engine-issues.md#native-composition-layer-teardown-crash).
The launcher still
completed successfully because it did not monitor the Android process throughout
the run. Check viewer logs and Android exit information when assessing success.

For a repeatable live motion and window-lifecycle exercise:

```sh
scripts/run-godot-hoist --app blender \
  --blender-script apps/weld-vr/tests/blender_motion.py --seconds 75
```

This starts factory-settings Blender, orbits the viewport, opens/closes
Preferences, and stops motion without saving user files. The Pico still emits
Godot GLES texture-cleanup errors when the secondary native window closes;
the same errors reproduced with latest-only presentation. They remain a
separate unresolved lifecycle issue. Display-aligned pacing, an adaptive
delay and explicit catch-up scheduling remain future experiments.

### Earlier validation

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
