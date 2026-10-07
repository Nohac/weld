# Dioxus Android texture probe

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

## Validated on Pixel 8 Pro

On 2026-10-07, the GLES build decoded and presented all 120 frames of its generated
320×180, 30 Hz AV1 clip. Replay, pause/resume, rotation, orientation and rounded
CSS clipping were visually confirmed. Background/resume released the converter,
recreated it on the same process's resumed renderer, and restarted playback.
The importer reported native-fence release rather than its synchronous fallback.
MediaCodec selected `c2.google.av1.decoder` for this small fixture, its software
AV1 implementation. This validates native decoder output import; hardware-decoder
selection still needs a representative larger clip and separate validation.

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

Host fixture tests and Android Clippy with warnings denied passed. These small
clips establish import and lifecycle feasibility; high-resolution sustained
playback, latency and thermal measurements remain migration prerequisites.

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
- The reused importer exposes an sRGB texture. Vello's current unorm composition
  path samples that texture into linear values without the matching output
  encoding, darkening midtones. A renderer-specific colour conversion or sampling
  adjustment is needed before judging image fidelity or migrating the app.
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
