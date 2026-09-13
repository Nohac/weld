# Godot native video

Native presentation is shared by the bounded fixture and the
[single-window Iroh viewer](godot-hoisting.md) and its [XR scene](godot-xr.md).
Godot 4.7.1 Compatibility runs OpenGL ES on Linux and Android. The Linux
`opengl3_es` override and runtime EGL/extension checks are deliberate: desktop
core OpenGL/GLX is not the validated image-import route. Vulkan external-memory
extension selection is tracked by [Godot PR #114940](https://github.com/godotengine/godot/pull/114940);
native import and synchronization still need implementation after that gate.

## Ownership

- `weld-media::DecoderConfig` (the light `config` feature) validates immutable
  codec setup independently of native APIs and the optional decode worker pool.
- `weld-media-android` owns FFmpeg/NDK contexts, a single-producer ImageReader
  target and movable acquired-image leases. No Godot, transport or pixel download.
  It is reused by the Android diagnostic probe and the Godot provider.
- The Godot app's `native` module selects Android or Linux providers at module
  boundaries. Linux uses the existing `FfmpegDecoder` and VPP XRGB output;
  Android uses MediaCodec PRIVATE/GPU-sampleable images. Decoder construction,
  calls and destruction stay on decoder workers (the fixture worker or shared
  decode pool, depending on the producer).
- `playback` shares the finite fixture driver, latest-output mailbox, pending
  presentation slot, counters and stop behavior. This fixture worker is not a
  replacement network scheduler or second multi-stream decode pool.
- `playback::render` and `egl.c` share render-thread EGLImage/texture handling.
  Provider imports differ: Linux DMA-BUF plus queried XRGB modifiers, Android
  native buffer. Godot owns the texture; its storage/size must not be changed
  after native import. Linux samples a 2D texture, Android an external texture.
  Rust retains the ExternalTexture and material through stop and queries the
  native ID itself; target-resource retention is structural in the Rust API.

Compressed input is not dropped between dependent frames. Only fully decoded,
never-presented outputs can be superseded in the one-slot latest mailbox. The
main thread selects the exact pending image and its crop before scheduling
render work; the render callback never rereads a newer mailbox image. Callable
captures contain native/shared state and numeric IDs, never Godot objects.

The Android producer fence is awaited on the codec worker. Images retain the
ImageReader independently of decoder lifetime; retaining just an AHardwareBuffer
would not prevent producer reuse. Linux finishes decode/VPP writes and exports
an independently owned XRGB allocation. Any future VPP output pooling must be
gated on the returned lease.

Replacement exports a native GPU release fence after prior Godot reads and
destroys the EGLImage view. The native lease stays in a bounded retirement
mailbox until a nonblocking main-thread fd poll proves completion. Normal
admission permits two retired frames; teardown has room for current/spare
imports too (four retirement entries maximum). No render-thread GPU wait,
`glFinish`, raw-frame upload or pixel download occurs in this path.

Stop clears the sampler, cancels/drains unused frames, queues render cleanup,
then uses a lifecycle-only rendering synchronization and at most two seconds of
main-thread fence waiting. Ordinary worker completion is polled; final exit joins
the worker so native/Rust code cannot unload beneath it. A native driver call
that hangs can still delay that final join. Context mismatch, fence failure or
timeout fails closed with a restart-required diagnostic and bounded retained
leases. Missing current context is never treated as proof of GPU completion.
Terminal TLS destruction performs no GL calls and retains unsafe-to-release
leases until process exit. Pause is Stop; resume requires explicit replay.

## Validation: 2026-09-12

- Linux Radeon 880M / Mesa 26.1.6: AV1 VLD advertised; real Godot Wayland/GLES
  playback twice, 120 decoded each, 117 then 120 presented. Screenshot confirms
  red top, blue bottom, white upper-left marker and visible content. Superseded
  frames are decoded mailbox replacements, not lost codec reference frames.
- One later fixed-six-second check observed one decoded frame and no presentation;
  its cause was not established. The smoke check now waits for completion within
  12 seconds per clip and excludes manual restarts. Three repeated two-clip runs
  then completed in 4.0-4.2 seconds each, all 120 decoded; one clip superseded
  44 outputs. This is recorded as an unexplained earlier test timeout, not a
  demonstrated production bug fix or startup-latency guarantee.
- Pixel 8 Pro / API37 / Mali-G715: Android AV1 presentation 120/120, user-confirmed
  video, correct visual crop/orientation markers, replay, pause during active
  playback (31 frames), resume stopped, replay again, and graceful Back exit.
  Clean APK startup has no Godot script errors. This is not color calibration,
  a latency benchmark, sustained-stream qualification or Pico validation.
- The refactored native Android probe still passes AV1, H.264 and VP9, 12 outputs
  each. AV1 uses empty extradata/in-band headers. Retained output remains valid
  after decoder destruction. See [codec qualification](android-codec-probe.md).
- Host tests cover fixture bounds/timestamps and nonblocking fd-readiness policy;
  workspace media tests cover shared decode-pool ownership. Device execution,
  not these host tests, validates GPU import and release fences.
- The Android export regression check asserts all native libraries, video panel,
  XR global-class metadata and autoload script dependencies. Godot's successful
  export exit code alone did not detect omitted resources in a scene-only export.

Run from the repository root after building:

```sh
apps/weld-vr/scripts/build-gdextension android
apps/weld-vr/scripts/check-gdextension --android-export
WELD_VR_VIDEO_CAPTURE=/tmp/weld-vr-video.png timeout 30s godot \
  --path apps/weld-vr --display-driver wayland --rendering-driver opengl3_es \
  --script res://tests/native_video_smoke.gd
```

The desktop test first submits one AU without EOS, then plays two full clips,
checks completion and exits. It records actual `frame_pre_draw` counts and
waits for drawing before starting, because process ticks alone do not prove
the window is being rendered. It still fails if no image is presented.
Its optional screenshot
is explicitly a diagnostic readback, not part of decoded-frame presentation.
Cargo's build script generates at most 120 fixture frames in `OUT_DIR` (30-second,
1 MiB limit) and records the encoder version; no video dumps or native
libraries are committed. Android API28 libraries are packaged in a development
APK whose prebuilt Godot manifest still says API24: **only install on ARM64/API28+
devices** until the manifest minimum is corrected before distribution.

The live viewer now connects the portable receiver and decode execution contracts
to this target; see its bounds and validation in [Godot hoisting](godot-hoisting.md).
Arbitrary negotiated codecs, multi-window presentation, rotation/context recreation
recovery and Vulkan import remain separate work.

## Live-receiver regression checks: 2026-09-13

The unchanged two-clip desktop smoke test passed at 120 decoded / 120 presented
for each clip after two earlier runs reported 120 / 0. The earlier environment
was not captured well enough to establish the cause. Disabling Godot's render
loop deliberately reproduces the same 120 / 0 signature; this is evidence that
the old assertion cannot distinguish missing draws from a presentation defect,
not proof of what happened during the earlier runs. Manual desktop Iroh
presentation was also confirmed by the user. No render-scheduler workaround was
added on the basis of the failed automated runs.

The updated test passed: single AU 1 decoded / 1 presented (10 draw callbacks),
then both full clips 120 / 120 (249 draw callbacks each). Initial drawing was
observed after 22 ms. The render-disabled negative control now fails before
decoding with zero draw callbacks and an explicit render-loop/visibility error.
These are one-run diagnostic observations, not timing guarantees.

Desktop single-AU coverage validates fixture/presenter plumbing only. The Pixel
was subsequently reconnected: a diagnostic APK with `-- --video-single-frame`
in its export arguments also decoded and presented exactly one frame, without
future input or EOS. Ordinary ADB intent extras did not work because Godot
sanitizes command-line parameters on exported activities; the diagnostic used
the existing fixture flag, not weakened intent sanitization. The normal export
arguments were restored afterward.

A fresh 45-second phone live run with the explicit public-DNS configuration
completed at 28 decoded / 28 presented / zero superseded, without the earlier
uninitialized `ndk-context` panic in the captured app log. This qualifies that
configuration on the tested phone/network, not Android system/Private DNS,
other hardware, all codecs or sustained high-frame-rate workloads.
