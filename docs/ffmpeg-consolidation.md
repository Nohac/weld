# FFmpeg frame ownership and the remaining VA-API boundary

Implemented and validated on 2026-11-10 with FFmpeg 9.0.2, the pinned
`ffmpeg-next`/`ffmpeg-sys-next` 9.0.0 bindings, and radeonsi exposing VA-API 1.24.

## Implemented

- FFmpeg owns packed BGRA input through an AVBuffer release callback. The existing
  pixel vector is transferred rather than copied into another CPU allocation.
- A persistent FFmpeg VA-API upload pool supplies frames for CPU-backed encoding.
  `av_hwframe_transfer_data` uploads the pixels; a DRM mapping retains the source
  surface through the ordinary encoding filter graph. DMA-BUF input keeps its
  existing path and both input types share the encoder generation.
- Decoded frames use `av_hwframe_map` with READ and DIRECT flags. FFmpeg owns
  synchronization, DRM export and mapping cleanup; the adapter no longer calls
  `vaSyncSurface` or `vaExportSurfaceHandle` itself.
- FFmpeg's separate R8/GR88 NV12 layers are normalized to the existing composed
  NV12 import representation. Plane/object indices, offsets, pitches and modifiers
  are preserved. Unsupported layouts and invalid indices are rejected.
- Mapped file descriptors are duplicated for Weld ownership. The mapped AVFrame
  and original decoded frame remain alive until VPP completion, preventing decoder
  surface recycling while the GPU reads them. Only the fresh XRGB output escapes
  to presentation.
- The encoder creates a direct VPP converter only for explicitly enabled
  source-image dumps. Hardware capability queries still use libva.

The private `weld-media-vaapi/src/ffmpeg/frames.rs` module owns frame wrappers,
release callbacks and DRM descriptor conversion. FFmpeg-native types remain inside
that media boundary. Decode timing's `decode_sync` now includes synchronization
and DRM export performed by the mapping operation; it is wall time, not a GPU
execution counter.

## Why modifier-controlled conversion remains

The installed `scale_vaapi` filter exposes no modifier-list option. FFmpeg's
[VPP setup](https://github.com/FFmpeg/FFmpeg/blob/master/libavfilter/vaapi_vpp.c)
creates and initializes its own output pool during filter graph configuration.
The generic VA-API frames structure has allocation attributes, but setting an
attribute on a separately created pool does not configure this filter's pool.

An isolated 128x128 NV12-to-XRGB conversion on `/dev/dri/renderD128`, followed by
FFmpeg's direct DRM mapping, returned one XRGB plane with modifier
`0x0200000010401b04`. That is not the linear modifier `0` requested by some of
Weld's consumers. It proves the default allocation cannot substitute for an
explicit linear requirement on this driver; it does not establish behavior on
other GPUs or rule out a future supported integration.

The decode path therefore retains the existing VPP converter, which accepts the
consumer's modifier candidates. Replacing it requires a supported constrained
allocation path through FFmpeg, an upstream filter enhancement, or retaining a
small direct VA-API processing layer. Changing FFmpeg's private filter state or
quietly accepting an incompatible modifier is not part of this migration.

The Rust FFmpeg bindings also expose `VASurfaceAttrib` as an opaque type, so
constructing VA-specific allocation attributes would need additional bindings.
Capability enumeration and codec-specific geometry queries remain another direct
libva boundary. These operations now live in `weld-vaapi`, whose safe API covers only queries,
DMA-BUF ownership, modifier-controlled conversion, and diagnostic image access.
Both vendored cros crates, the old diagnostic codec and its comparison tools were
removed on 2026-11-11. Adapted resource-lifetime code retains the upstream BSD
license and provenance in `crates/weld-vaapi/UPSTREAM.md`. Bindings are generated
from installed libva >= 1.23 headers (package version 2.23), including only the
required ABI. No codec parameter structures are maintained by Weld.

## Validation

The unit tests cover packed-buffer ownership without a copy, malformed extents,
composed versus separate-plane DRM layout normalization, and invalid plane
references/offsets. Run these without a GPU:

```sh
cargo test -p weldwm -p weld-media-vaapi --lib ffmpeg
```

The opt-in offscreen hardware test compares FFmpeg uploads against the existing
libva upload for H.264 and AV1. It uses small even extents that exercise row
alignment and AV1 minimum-size padding, checks every visible pixel against the
reference, checks timestamps and linear output modifiers, and reads retained
outputs after later submissions and codec destruction. It creates no desktop
windows. Select the render node explicitly:

```sh
WELD_TEST_RENDER_NODE=/dev/dri/renderD128 cargo test \
  -p weldwm -p weld-media-vaapi -p weld-vaapi --lib -- --ignored --nocapture
```

PPM evidence is written to `target/validation/ffmpeg-consolidation/`. The initial
96x72 uniform-color comparison was byte-identical between upload paths for all
six decoded outputs. The final 98x74 gradient comparison also matched every
visible channel exactly for both codecs, while preserving linear modifiers and
outputs after codec retirement. This is equivalence to the existing path, not a lossless
codec or color-fidelity claim: the first comparison with original saturated RGB
values exposed the same deviation in both paths. Performance is not established
by these bounded correctness tests.

The utility also checks exact mapped-image upload/readback after converter
retirement; that test passed. Successful extraction runs reproduced all twelve
saved pre-extraction PPMs byte for byte. Repeated untraced comparisons exposed an
intermittent AV1 frame-1 edge difference of 33 channel levels in both binaries:
the unchanged binary failed twice and passed once; the extracted binary passed
once, failed the same pixel check once, and aborted once with a radeonsi hard
recovery during FFmpeg AV1 encoding. GPU tests were then stopped. This validates
neither stable AV1 driver behavior nor a clean hardware suite. The pixel test
retains its strict tolerance; resolving the AV1 instability remains separate
from the utility extraction. Logs are under `target/validation/vaapi-utility-*`.
