# Android native-buffer codec qualification

Implemented diagnostic, not a production Android decoder or Godot presenter.
The portable receiver/worker extraction remains unchanged. This probe answers
whether the selected FFmpeg MediaCodec decoder can produce independently
acquired Android native images without Weld reading pixels back to the CPU.

## Run

From the shared Rust development shell:

```sh
scripts/build-android-codec-probe
scripts/run-android-codec-probe --serial DEVICE_SERIAL --codec av1
scripts/run-android-codec-probe --serial DEVICE_SERIAL --codec h264
scripts/run-android-codec-probe --serial DEVICE_SERIAL --codec vp9
# Query the exact selected_codec reported by a run, not the FFmpeg wrapper name:
scripts/run-android-codec-probe --serial DEVICE_SERIAL --codec-info c2.google.av1.decoder
```

The launcher requires an explicit ARM64 device running API 28 or later. Codec
flags require API 29; older devices report them as unavailable. Nothing is
installed as an APK and no Godot, networking or device settings are changed.
Each invocation leaves its named diagnostic directory under
`/data/local/tmp/weld-codec.*`; decode runs copy about 10 MB, mostly Rust debug
symbols. The launcher prints the exact directory. Host logs and generated
fixtures live under `target/android-codec-probe/`. None are committed.

`app_process` starts Android's runtime and Binder pool, then a tiny Java entry
point loads the libraries and calls Rust. All decoding and native-image handling
use FFmpeg/NDK; no FFmpeg JavaVM registration is needed. The separate Java
metadata mode queries `MediaCodecList(ALL_CODECS)` without creating a codec. It
prints hardware/software/vendor flags and alias/canonical-name information.
This diagnostic process does not establish normal APK permissions or lifecycle
behavior.

## Build and safety bounds

- FFmpeg 8.1.2 is pinned to commit
  `38b88335f99e76ed89ff3c93f877fdefce736c13`, with archive SHA-256
  `2ae7e42343cfffb811d15cfe98b6d005f082595fcdf034d30a4ff90cfed9f9c6`.
  `scripts/build-android-ffmpeg` builds minimal ARM64/API28 shared libraries
  with two jobs. It checks enabled components, license configuration,
  SONAMEs and Android dependencies before publishing a completed cache entry.
  Failed/intermediate build directories are retained, not recursively cleaned.
- The isolated Rust workspace uses the same `ffmpeg-next` **9.0.0** version as
  production, with only codec/format features. It uses `weld-media` codec
  identities, not Linux compositor/VA-API dependencies. Native prefix and bindgen
  sysroot are explicit; host FFmpeg libraries must not enter the Android link.
- These API28 libraries are **not** promised loadable in the current API24
  Godot app. Decide the production minimum API before packaging them there.
- Fixtures are regenerated from `testsrc2`, 320x180, twelve low-delay access
  units each: AV1 Main, H.264 Constrained Baseline and VP9 Profile 0. The host
  FFmpeg version is recorded beside them. Software fixture encoding avoids
  stressing the host GPU. The probe rejects files above 16 MiB, more than 120
  packets, unexpected streams and extents above 1920x1088.
- FFmpeg log capture is capped at 256 KiB. Input retries and receive calls are
  bounded; image delivery has a two-second deadline, fence waits one second and
  EOS drain three seconds. A separate 30-second watchdog uses `_exit` without
  native destructors if a native call hangs. That emergency path explicitly
  skips graceful cleanup; it does not cancel a GPU job safely.

## Buffer and output behavior

FFmpeg stream inspection is forbidden from opening a decoder by setting a
deliberately empty-in-practice codec whitelist. Its resulting whitelist warnings
are expected. Parsers still obtain dimensions and H.264 SPS/PPS. This prevents
an implicit decoder from running before the native output target exists.

The explicit decoder receives an `AVMediaCodecDeviceContext` pointing at an
ImageReader-owned native window. `ndk_codec=1` is read back after open, and
`get_format` accepts only `AV_PIX_FMT_MEDIACODEC`. The reader requests PRIVATE
images with GPU-sampling usage and at most four acquired images; the probe holds
only one at a time.

The probe preserves packets across send EAGAIN, tracks accepted microsecond
timestamps separately from decoded frames, and drains delayed output through
EOS. A first-packet idle observation checks whether an output arrives before
another packet or EOS. It is not a latency or pipeline-depth benchmark.

An opaque FFmpeg frame is rendered to the native window, then the probe acquires
the next ImageReader image, waits its acquire fence and checks timestamp,
hardware-buffer metadata and crop. It never maps planes, locks pixels, copies
YUV/RGB or calls a hardware-frame download API. A failed fence wait transfers
that fence back with the image rather than discarding producer readiness.
Images and frames do not escape the session. Destruction releases these before
the decoder, device reference, retained native window and reader. No GPU sampling
is submitted, so this does **not** validate a presenter's GPU release fence.

The native codec name comes from FFmpeg's success log. MIME-name fallbacks are
unknown, known software prefixes are reported as software, and every other name
is still hardware-unverified until separately queried. Native-surface output
alone does not prove hardware decoding.

## Phone evidence: 2026-09-11

Pixel 8 Pro, Android API37, ARM64; FFmpeg8.1.2 and NDK29. All three finite
sequences passed with 12 decoded frames, 12 acquired GPU-sampleable images,
matching timestamps and the expected 320x180 crop.

| Codec | Actual selected decoder | Reported hardware / software / vendor | AHB storage / format |
| --- | --- | --- | --- |
| AV1 | `c2.google.av1.decoder` | true / false / true | 320x192 / 769 |
| H.264 | `c2.exynos.h264.decoder` | true / false / true | 320x192 / 291 |
| VP9 | `c2.exynos.vp9.decoder` | true / false / true | 320x180 / 291 |

Flags were queried from this device's MediaCodecInfo entries, not inferred from
names or measured GPU counters. These vendor AHB formats still need qualification
for Vulkan external-format/YCbCr import. The first-packet observations produced
an image without subsequent input/EOS for all three codecs. Their startup times
are not steady-state frame latency; this short sequence proves neither 60-fps
throughput, multi-stream capacity nor sustained resource stability.

No Pico decoder was exercised. No image was displayed in Godot, and color,
orientation, visual crop accuracy, renderer synchronization and pause/resume
remain unvalidated. No production codec, transport or wire behavior changed.

## Checks and next boundary

```sh
cargo test --locked --offline --manifest-path tools/ffmpeg-android-probe/Cargo.toml \
  --target-dir target/android-codec-probe/cargo -j2
cargo clippy --locked --offline --manifest-path tools/ffmpeg-android-probe/Cargo.toml \
  --target-dir target/android-codec-probe/cargo --all-targets -j2 -- -D warnings
scripts/build-android-codec-probe --clippy
```

Host tests cover timestamp bookkeeping, limits and conservative name
classification without linking FFmpeg. Android build/Clippy check real native
code; the device runs above are separate execution evidence.

Next, use this evidence to generalize existing FFmpeg session/packet handling
and provide an Android output adapter. The production worker must accommodate
delayed output without requiring another client commit. Published native-image
leases must remain valid through renderer use and backend retirement, not merely
retain an opaque codec output index. Prove Godot GPU-native presentation before
claiming a working phone hoist; its current Vulkan import remains a separate
capability and ownership gate. Keep this probe isolated until that reusable
backend can replace its diagnostic session code.

Source references: [pinned FFmpeg MediaCodec decoder](https://github.com/FFmpeg/FFmpeg/blob/38b88335f99e76ed89ff3c93f877fdefce736c13/libavcodec/mediacodecdec.c),
[Android runtime bootstrap](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/cmds/app_process/app_main.cpp),
[NDK media APIs](https://developer.android.com/ndk/reference/group/media).
