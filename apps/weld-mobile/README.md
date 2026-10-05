# Weld Mobile receiver proof

An Android ARM64 Bevy shell presenting one streamed application. It reuses
`weld-hoist-iroh`, `weld-hoist-encoded`, `weld-client`, the shared decode-worker
pool, and `weld-media-android`. The Android activity and presentation adapter
live here; the Linux compositor stays on the source machine.

## Run

For pairing with your running desktop, application selection and revocation,
follow [Phone pairing](../../docs/device-pairing.md). The commands below launch
the separate development fixture.

With the Rust/Android development shell and one authorized ADB device:

```sh
scripts/run-mobile-hoist
scripts/run-mobile-hoist --seconds 300 -- foot
scripts/run-mobile-hoist --codec h264
```

The launcher builds the debug APK and ordinary development host, installs the
APK, and starts a private headless Foot/htop session. It exchanges public
identity/profile files over ADB; video and input use Iroh N0 over IP. Internet
discovery/relay services are contacted, and the development Android receiver
uses public DNS rather than Android's Private DNS configuration. Private keys
stay in each device's private storage. Existing host applications, routes and
desktop configuration remain unchanged.

`--serial` chooses a device; `--no-build` uses the installed APK and host build.
The default test lasts 120 seconds; `--seconds` accepts 1..1800. Ctrl-C stops
the owned application session and phone app. Logs are in
`target/validation/mobile-hoist-*`. Launching another test on the same device
requires stopping the first one. Tests and lint checks are separate from launch.

Build/install only:

```sh
scripts/build-mobile
scripts/build-mobile --install --serial DEVICE
scripts/build-mobile --clippy
```

Rust builds use `cargo-ndk`. A pinned Gradle wrapper packages the small Android
activity and Google Code Scanner dependencies; the SDK supplies Nix-compatible
`aapt2`, `zipalign` and `apksigner`. Android Studio is unnecessary. The first build
downloads Gradle and Android dependencies; later builds reuse their caches.
Build outputs use `target/mobile`; the ignored `apps/weld-mobile/.local`
directory keeps the development signing key across target cleanup. Preserve
that key to update an installed APK without uninstalling its private identity.
The APK is debuggable and uses development signing only.

## Presentation and input

The phone requests tiled sizing and preferred scale from its full display and
Android density, including the camera-cutout area. Pairing/browser controls use
safe insets. Android Back returns a paired application to the desktop; Back from
the application list backgrounds Weld Mobile. There is no reserved control strip.

Sizing is sent as soon as the selected surface is announced. Initial presentation
holds at most one latest native image until matching logical geometry arrives;
after one second it presents the latest available image for applications that
choose another size. That fallback also fires when the app stops committing.
Rotation updates requests after a 150 ms settling interval. Later resizes remain
live and aspect-preserving, and size increments do not cause configure loops.

The Pixel full-display check requested 448x997 logical pixels at 3x scale and
first presented matching geometry 312 ms later, without first publishing the
cached desktop-size frame. Steam fit was confirmed manually. Fifteen Android
tests cover geometry, startup fallback, Back/return and input/mailbox behavior;
they passed on the Pixel. Android Back dispatch/backgrounding was exercised on
the development fixture; paired return policy is also covered by a focused test.

The video/touch area includes the camera cutout; browser controls respect
Android's stable system-bar and camera-cutout insets. The insets adapter samples
the Android UI thread asynchronously at most twice a second, with an immediate
refresh on window-size changes. Presentation waits for matching-size insets.
Large viewport requests use a provisional shell size budget. The codec validates
its actual supported extent when opened; there is no hardcoded 4096-square
hardware claim. Proactive codec-capability negotiation remains future work.
Contacts carry stable IDs and down/motion/up/cancel/frame events through the
shared runtime. The host chooses native Wayland touch for applications binding
it, otherwise primary-finger mouse emulation. Both choices stay fixed through
the gesture. The shared surface-input geometry handles crop/scale coordinates
and input regions, capturing each contact's initial target. Focus loss,
cancellation, rotation/mapping changes and queue overflow cancel held contacts.
An epoch guards
input and decoded-frame publication across window unmap/destruction.

The receiver selects the first toplevel and requests 60 Hz while active. Other
surfaces receive paused presentation demand. This proof presents only the root
layer: window switching, popups/subsurfaces, keyboard/IME, remote close/reclaim
controls, and automatic reconnect are subsequent work. Existing protocol and
receiver capabilities remain shared; those limitations belong to this shell.

Android's decoder supplies acquired native images. Under wgpu's GLES context
lock, one conversion pass samples the external video image into a retained
sRGB GPU texture. Bevy samples that texture; decoded pixels are never downloaded
to CPU memory. A native fence returns each acquired image to Android after its
last conversion read. The output texture is reused until size changes; a size
change invalidates Bevy's image bind group. Pipelined Bevy rendering is disabled
for this initial single-context integration. A fence-export failure falls back
to synchronous GPU completion with a warning; it never downloads pixels.

The decode adapter now lives in `weld-hoist-encoded::android` and uses the shared
pool defaults: up to four workers, eight jobs, sixteen generation slots and
two in-flight jobs per worker. These are admission budgets, not hardware
capabilities. Same-generation input can be submitted while the preceding output
is pending; reordered native outputs are retained against their exact job.
ImageReader waits and decoder jobs
have bounded deadlines. Foreground loss pauses source frame demand while the
control connection remains available; Android may still suspend/kill background
processes, which this proof does not automatically recover from.

Presentation uses `weld-client::PresentationMailbox`, extracted from the XR
receiver and also used by desktop Weld. It keeps at most two native images,
skips stale history after a stall, and shows a final frame without prefill.
Resize/input-layout changes invalidate queued history. The coordinator retains
protocol leases; only owned native images cross to the Bevy adapter. Queue
locks are released before native-image destruction or GPU conversion.

The Bevy setup and image publication helpers are shared with desktop Weld.
The phone omits Bevy's multithreaded schedule feature and wakes Winit for new
frames, UI changes and input instead of continuously redrawing unchanged video.
A 100 ms foreground fallback services viewport settling and Android insets.
`weld_mobile_diag` reports bounded publication, selection/drop and GPU-conversion
timings. These measure individual stages, not end-to-end latency or scanout.

## Validation, 2026-10-04

- Debug APK built and installed on a Pixel 8 Pro (Android API 37).
- Bevy used the Mali-G715 GLES renderer. Live H.264 and AV1 reached the display.
- H.264 selected `c2.exynos.h264.decoder`. AV1 selected `c2.google.av1.decoder`.
  A direct Android `MediaCodecInfo` query confirmed the AV1 implementation reports
  `hardware=true software=false vendor=true`. The vendor capability file declares
  a 3840x2160@60 performance point; the sizing test used a portrait window around
  1344x2577 and a landscape window around 2841x996 (subject to terminal increments).
  The codec name's `google` prefix does not imply software decoding.
- AV1 is the launch default. Android reports no low-latency feature for that
  codec; hardware acceleration and low-latency mode are separate capabilities.
- User confirmed rotation and tap interaction. Screenshots confirmed the
  corrected aspect ratio after selecting `NodeImageMode::Stretch`.
- Phone-sized AV1 passed portrait/landscape interaction. The final safe-area
  build requested 448x859 and 947x332 logical pixels at 3x scale on this phone;
  rotation returned to portrait without reconnecting or codec failure.
- A background/foreground cycle produced source `Paused` then active 60000 mHz
  presentation demand and resumed updates on the existing connection.
- Android and desktop Clippy with warnings denied passed. Seven focused Rust tests were
  compiled for ARM64 and executed on-device; no emulation was used.
- Launcher tests cover log-capture timeout and cleanup after device failure.

The Mali driver emits non-AFBC allocation performance diagnostics through
wgpu's error logger during startup. Android/N0 relay connection attempts also
emitted deadline warnings; the validated stream selected a direct IPv4 path.
Internet-only relay operation, long background suspension and additional phone
GPUs remain unqualified.

## Shared presentation validation, 2026-10-05

The shared-handoff APK passed startup and a paired Steam scrolling test on the
Pixel. A 12.084 s active sample published 722 frames and converted/published
721 to Bevy (59.67/s), with one capacity supersession, no stale or lifecycle
discards, and no reported codec failure. Mean handoff age was 3.18 ms (15.10 ms
maximum); mean conversion-call time was 1.47 ms (3.21 ms maximum). A later
isolated update was presented without another frame. These are receiver-stage
observations, not measured input-to-photon latency or a guarantee of evenly
spaced scanout. The phone still lacks explicit 60-to-120 Hz phase alignment.

The private AV1 Weld-to-Weld fixture also completed its 25-second run. Portable
queue tests cover retained buffers, lifecycle barriers, final-frame delivery,
drop accounting and native release outside locks. Godot's 109 Rust tests pass;
a new physical XR regression and sustained image-budget stress remain unqualified.

## Native touch validation

`examples/touch-probe.html` distinguishes native multi-touch from mouse events
and includes a finger-scrollable list. Open it in a private browser launched by
`scripts/run-mobile-hoist`. On Pixel 8 Pro, Chrome recorded four simultaneous
contacts, 20 matching down/up events, zero mouse presses and scrolling. The
probe used `--disable-gpu --ozone-platform=wayland` because this Chrome build's
GPU buffers hit the source VA-API import format whitelist. That limitation is
separate from native touch. Firefox uses subsurfaces which this phone shell does
not yet present. The host's native protocol tests also cover cancellation,
controller isolation, first-seat compatibility and pointer fallback.

## Next product slices

Device pairing and an authorized running-window browser are implemented.
An explicit host-owned remote-launch catalog and automatic keyboard/IME remain
future work. Touch input is sufficient for the current flow. The test launcher's
whole-private-session consent remains separate from ordinary desktop approval.
