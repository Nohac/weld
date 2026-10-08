# Dioxus native texture and streaming probe

This isolated APK validates decoded video inside a CSS-styled Dioxus Native / Blitz
interface. Build it from Weld's Android development shell:

```sh
scripts/run-dioxus-texture-probe --install
# With multiple ADB devices:
scripts/run-dioxus-texture-probe --install --serial DEVICE_SERIAL
```

The app installs as **Weld Dioxus Probe**, package `org.weld.dioxusprobe`. Build
artifacts and its debug signing key live under `target/dioxus-texture-probe`.
Deleting that key requires uninstalling the old probe before installing a newly
signed APK. The launcher builds, packages and starts the probe; checks run
separately with `--check`.

## Live streaming on Linux and Android

```sh
# Linux receiver; isolated headless source starts Foot/htop:
scripts/run-dioxus-stream-probe --desktop
# Android receiver; builds and installs the separate probe APK:
scripts/run-dioxus-stream-probe --serial DEVICE_SERIAL
# Repeatable moving 1080p60 workload (also works with --desktop):
scripts/run-dioxus-stream-probe --serial DEVICE_SERIAL --seconds 120 -- \
  mpv --no-config --no-audio --vo=gpu --gpu-context=wayland \
  'av://lavfi:testsrc2=size=1920x1080:rate=60'
```

Use `--codec h264` for the comparison, `--width`, `--height` and `--fps` for the
requested source geometry/cadence, and `--bitrate-mbps` for the encoder budget.
Defaults are 1920×1080, 60 Hz and 8 Mbps AV1. `--seconds` defaults to 120;
Ctrl-C stops the owned processes. `--no-build` reuses the installed APK or Linux
binary. Launching runs builds only; tests and Clippy are separate checks.
GPU completion polling is nonblocking by default on both targets. Use
`--blocking-poll` to reproduce the original completion-wait baseline.

The source is a private headless Weld session using the usual Iroh admission
and public-profile exchange. N0 discovery/relays contact the Internet. The probe
requests a composed stream and displays its first toplevel. Application input,
pairing UI and multiple toplevel presentation belong to the later client slice.
The only interactive probe control is Pause/resume.

Both targets use `ClientRuntime`, the encoded destination, production decode
workers and `PresentationMailbox`. Android uses `AndroidDecodeBackend`; Linux
adapts `VaapiDecodeWorker` and imports its linear XRGB DMA-BUF through EGL.
The widget registers ordinary WGPU textures with Blitz and wakes on frame
arrival. A remaining jitter slot requests another draw so the final frame can
arrive without further source input. Suspension clears pending images and
pauses source presentation demand. Linux keeps the imported allocation alive
through the preceding submission's completion; Android releases through its
native fence.

### Measurements and evidence

The 2026-10-07 1080p60 AV1 tests displayed correctly on Linux and Pixel 8 Pro;
the user confirmed both looked smooth. H.264 also streamed on the Pixel.
The probe and dependencies use dev opt-level 3, with WGPU debug/validation
disabled for the final measurements.

Linux's final steady sample selected 59.94 frames/s, with average import-call
time 0.089 ms and decode-completion-to-import-start age 2.41 ms. Its steady
sample had zero superseded/stale mailbox entries. A process sample showed
about 20% of one CPU core and 145 MiB RSS. Adapter and loaded-library evidence
confirm radeonsi / Mesa 26.2.3.

The Pixel's final steady sample selected 56.67 frames/s. Import calls averaged
1.916 ms and decode-completion-to-import-start age 14.18 ms; mailbox replacements
and stale entries remained. A process sample was
143% CPU (1.43 core equivalents), about 274 MiB PSS / 408 MiB RSS, with thermal
status 0. This establishes working GPU presentation while leaving meaningful
phone performance work. CPU samples and short thermal observations are snapshots;
a matched production-app comparison and longer thermal run are still needed.
Steady aggregates omit the first five and last two nonzero reporting intervals;
intervals exceeding 1.5 seconds are excluded as startup/suspension gaps.

Logs are in `target/validation/dioxus-stream-*`. Final Linux evidence is
`dioxus-stream-4jgwq2re`; final Android evidence is `dioxus-stream-k6wijkdf`.
`dioxus-stream-yp3hupq7` also exercised Home/background followed by successful
resume on the same connection. Source and receiver logs retain the ordinary
codec and network diagnostics. `cpu.txt`, `memory.txt`, `thermal.txt` and
`graphics-libraries.txt`, where present, are manual samples from these runs.

Each `probe_present` record contains interval selected-frame counts and summed
CPU-side import durations, mailbox age and decode-completion-to-import age.
Divide sums by the corresponding sample count. Superseded/stale/submitted are
cumulative mailbox counters. These boundaries end before GPU execution and
physical scanout; network transit is described separately by transport logs.

Linux requires a probe-local AnyRender display-handle fix documented in
[the vendor note](../../vendor/anyrender-vello-hybrid/README.weld.md). Remove it after
the upstream GLES initialization path accepts the native display. Runtime
library paths preserve the caller's existing paths before pkg-config fallbacks.

### Pixel pacing investigation

The follow-up compared `--queue smoothing` (the default) with `--queue latest`,
then independently changed the renderer's completion policy using
`--nonblocking-poll`. Nonblocking polling is now the default on both targets;
`--blocking-poll` retains the baseline comparison. The change stays within this
isolated probe.

On the same 1080p60 AV1 workload, with the Pixel display reporting 120 Hz:

| Queue | GPU completion | Selected frames/s | Mailbox age | Decode-to-import age |
| --- | --- | ---: | ---: | ---: |
| Smoothing | Blocking | 57.54 | 13.18 ms | 13.49 ms |
| Latest | Blocking | 54.16 | 5.86 ms | 6.16 ms |
| Latest | Nonblocking | 59.90 | 0.82 ms | 1.13 ms |
| Smoothing | Nonblocking | 59.97 | 0.88 ms | 1.19 ms |

Removing the queue's history alone trades frame retention for lower age. The
main bottleneck was AnyRender's unconditional `Device::poll(wait_indefinitely())`
after every presentation. Its timed baseline spent 7.95 ms/frame there, alongside
6.08 ms of scene construction (including video import). Nonblocking `Poll`
reduced completion polling to 0.034 ms/frame. Presentation-call time changed
from 1.10 to 1.36 ms/frame, so the removed wait was largely avoided rather than
transferred to that call. Smoothing plus nonblocking had zero steady-state
superseded/stale entries in the measured interval.

```sh
scripts/run-dioxus-stream-probe --serial DEVICE_SERIAL --nonblocking-poll -- \
  mpv --no-config --no-audio --vo=gpu --gpu-context=wayland \
  'av://lavfi:testsrc2=size=1920x1080:rate=60'
```

`probe_paint` records paint intervals and worker-request-to-widget-entry time.
Follow-up redraws used to drain the jitter slot do not stamp the worker wake
clock. `probe_render` splits scene construction, surface acquisition, command
encoding, queue submission, presentation and completion polling. Each duration
is wall time around that call; GPU execution can overlap the CPU after polling
becomes nonblocking. Pause/startup windows should be excluded using elapsed
time and the lifecycle logs.

Evidence: `dioxus-stream-d1rnz140` (smoothing baseline),
`dioxus-stream-uern0ugi` (timed latest baseline), `dioxus-stream-t4e88gft`
(latest/nonblocking), and `dioxus-stream-4vtzxrti` (smoothing/nonblocking).
The first baseline wake-clock implementation also counted self-requested draws;
use those baseline runs for frame/queue/renderer metrics, and the corrected
nonblocking runs for worker wake latency. Aggregates omit the first six and
last two report windows. These short runs establish the pacing cause; longer
thermal and GPU-completion lifecycle validation remains useful before adopting
the renderer change in a production client.

## Validated on Pixel 8 Pro

On 2026-10-07, the GLES build decoded and presented all 120 frames of its generated
320×180, 30 Hz AV1 clip. Replay, pause/resume, rotation, orientation and rounded
CSS clipping were visually confirmed. Background/resume released the converter,
recreated it on the same process's resumed renderer, and restarted playback.
The importer reported native-fence release rather than its synchronous fallback.
MediaCodec selected `c2.google.av1.decoder`. Android's queried flags identify it
as `hardware=true software=false vendor=true` on this Pixel. See
[decoder-name classification](../../docs/android-codec-probe.md#decoder-name-classification-trap)
before interpreting codec names or CPU usage.

The path is:

```text
Embedded AV1 → weld-media-android → AHardwareBuffer / EGLImage
            → GLES conversion → WGPU texture → Blitz custom widget
```

Weld's existing Android decoder and GLES conversion C source are reused. The
presenter imports decoder output without CPU readback; GPU conversion produces
an ordinary texture that Vello samples alongside the CSS interface. The worker
has a two-image queue and stops
when the widget suspends or starts another playback. Fixture dimensions are
checked before import; adapting this to live streams requires updating the
registered resource when dimensions change.

Host fixture tests and Android Clippy with warnings denied passed. Live-stream
measurements above extend the initial fixture validation.

## Renderer findings

- Blitz is pinned to `d1b433edbc13da889228ae4f92908f7c28514019`, using Vello Hybrid
  and WGPU 30. The `custom-widget` feature is enabled explicitly for DOM, paint
  and shell so both painting and Android suspend callbacks reach the widget.
- On this Pixel, the initial Vulkan launch crashed inside the Mali shader compiler
  while Vello initialized its graphics pipeline, before video decoding started.
  The stack included `spir2lir::image_addr_is_yuv`, `vkCreateGraphicsPipelines`
  and `vello_gpu::render::wgpu::Renderer::new_with`. Root cause is unconfirmed.
  The probe selects GLES through Android's debuggable `wrap.sh` mechanism.
  Streamed ADB installation is necessary here: incremental installation omitted
  extraction of the wrapper during testing.
- The initial fixture exposed an sRGB texture, darkening midtones in Vello's
  unorm composition path. The current probe reuses Weld's image import and fence
  handling with a small encoded-colour shader and RGBA8 allocation adapter.
  The production phone importer remains unchanged. Colour-managed/HDR output
  still needs its own validation.
- Blitz can call the widget readiness hook through both window resume and first
  paint; creation is idempotent. Resource IDs are unregistered on resume through
  the live renderer context.

## UI component compatibility

The probe uses ordinary Dioxus elements, CSS and Rust event handlers. First-party
Dioxus Components require individual validation: simple styled elements are a
good fit, while primitives including dialogs, popovers and checkbox behaviour
use `document::eval`. The pinned Native implementation delegates that API to
`NoOpDocument`, returning an unsupported error. Those interactions need native
implementations before the corresponding components can be relied upon.

Relevant upstream sources:

- [Blitz texture-widget example](https://github.com/DioxusLabs/blitz/tree/d1b433edbc13da889228ae4f92908f7c28514019/examples/wgpu_texture)
- [Native document implementation](https://github.com/DioxusLabs/blitz/blob/d1b433edbc13da889228ae4f92908f7c28514019/packages/dioxus-native/src/contexts.rs)
- [Dioxus Components](https://github.com/DioxusLabs/dioxus-components)
- [Android wrap.sh packaging](https://developer.android.com/ndk/guides/wrap-script)
