# Weld Mobile receiver proof

An Android ARM64 Bevy shell presenting one streamed application. It reuses
`weld-hoist-iroh`, `weld-hoist-encoded`, `weld-client`, the shared decode-worker
pool, and `weld-media-android`. The Android activity and presentation adapter
live here; the Linux compositor stays on the source machine.

## Run

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

Packaging uses the SDK's `aapt2`, `zipalign` and `apksigner`, plus `cargo-ndk`;
Android Studio and Gradle are unnecessary for this NativeActivity bootstrap.
Build outputs use `target/mobile`; the ignored `apps/weld-mobile/.local`
directory keeps the development signing key across target cleanup. Preserve
that key to update an installed APK without uninstalling its private identity.
The APK is debuggable and uses development signing only.

## Presentation and input

The source starts at 960x640, then the phone requests a logical window size and
preferred scale from its available area and Android display density. Rotation
updates those requests after a 150 ms settling interval. Preferences use a
latest-value mailbox; application size increments do not cause configure loops.
The received content keeps its aspect ratio while a resize is pending or an
application chooses a different size.

Android stable system-bar and camera-cutout insets, plus a 64 logical-pixel
status strip, remain outside the video/touch area. The insets adapter samples
the Android UI thread asynchronously at most twice a second, with an immediate
refresh on window-size changes. Presentation waits for matching-size insets.
Large viewport requests are reduced to leave room inside the decoder dimension
ceiling; this is a shell bound, not hardware capability negotiation.
One finger maps to primary-button press, captured motion and release. The
shared surface-input geometry handles crop/scale coordinates and input regions.
Focus loss, cancellation, rotation/mapping changes and queue overflow release
held input. An epoch guards
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

The decode adapter uses the shared bounded, stream-affine pool with two workers,
one outstanding job per worker, and eight generation slots. Throughput tuning
and pipelined decode remain to be qualified. ImageReader waits and decoder jobs
have bounded deadlines. Foreground loss pauses source frame demand while the
control connection remains available; Android may still suspend/kill background
processes, which this proof does not automatically recover from.

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

## Next product slices

Device pairing UI should replace the launcher file exchange, followed by an
authorized running-window browser and an explicit host-owned remote-launch
catalog. Touch input is sufficient for the next slices; automatic keyboard/IME
integration is deferred. Pairing, catalog visibility, launch and
input/hoist authorization remain separate capabilities. The test launcher's
whole-private-session consent is not that product authorization flow.
