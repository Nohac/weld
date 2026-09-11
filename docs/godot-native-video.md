# Godot native video

Implemented bounded fixture presentation, not a phone hoist or XR session.
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
  calls and destruction all stay on the fixture worker.
- `playback` shares the finite fixture driver, latest-output mailbox, pending
  presentation slot, counters and stop behavior. This fixture worker is not a
  replacement network scheduler or second multi-stream decode pool.
- `playback::render` and `egl.c` share render-thread EGLImage/texture handling.
  Provider imports differ: Linux DMA-BUF plus queried XRGB modifiers, Android
  native buffer. Godot owns the texture; its storage/size must not be changed
  after native import. Linux samples a 2D texture, Android an external texture.
  The current panel retains its ExternalTexture through stop; Rust retains the
  material but receives the texture as a native ID. Before adding more callers,
  make target-resource retention structural in the Rust presentation API.

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

The desktop test plays twice, checks completion and exits. Its optional screenshot
is explicitly a diagnostic readback, not part of decoded-frame presentation.
Cargo's build script generates at most 120 fixture frames in `OUT_DIR` (30-second,
1 MiB limit) and records the encoder version; no video dumps or native
libraries are committed. Android API28 libraries are packaged in a development
APK whose prebuilt Godot manifest still says API24: **only install on ARM64/API28+
devices** until the manifest minimum is corrected before distribution.

Next: connect the existing portable receiver/encoded-port and decode execution
contracts to this presentation target. Networking, arbitrary negotiated codecs,
multi-window presentation, dynamic extents, rotation/context recreation recovery,
Pico/OpenXR and Vulkan import are outside this fixture slice.
