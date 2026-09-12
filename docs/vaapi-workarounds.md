# VA-API encoding workarounds

These are implemented Linux codec-backend safeguards, not AV1 specification
limits or a complete driver qualification system. They apply identically to
local, Iroh, nested-source and headless-source encoding. Window geometry,
input coordinates and transported visible extents do not change.

## AV1: even encoded picture dimensions

`weld-media-vaapi/src/probe.rs`, `VaapiEncodeGeometry::coded_extent`, rounds
AV1 width and height upward to even numbers **after** applying minimum sizes.
The maximum is checked again after rounding. Overflow or a rounded extent
above the maximum is rejected; there is no fallback to the unsafe odd extent.
H.264 geometry is unchanged.

`FfmpegEncoder` uses the existing `pad_vaapi` path to add black pixels on the
right/bottom, without scaling the original picture. `PendingDecodedFrame::finish`
crops decoded storage to the original visible rectangle before presentation.
The extra alignment costs at most one row and one column beyond minimum padding.
This is not a lossless-color claim. The grayscale probe only validates luma
and geometry; a remaining padded-edge luma deviation is described below.

### Why it exists

On 2026-09-12, FFmpeg 8.1.2 with Mesa 26.1.6/radeonsi on AMD Strix
Radeon 880M/890M reset the VCN engine when encoding a 1280x833 picture. The
failure reproduced in plain FFmpeg without Weld, a decoder, a resize or a network.
The kernel attributed a `vcn_unified_0` timeout to FFmpeg; its backtrace showed
the encoder waiting in `vaSyncBuffer`. Mesa's submission worker then aborted
the non-robust context after the reset. The abort is a consequence, not the
initial faulty firmware operation.

The relevant [Mesa 26.1.6 AV1 session setup](https://gitlab.freedesktop.org/mesa/mesa/-/blob/mesa-26.1.6/src/gallium/drivers/radeonsi/radeon_vcn_enc.c#L906)
checks source-allocation padding against `alignment - 2`, then increases it with
`MAX2(aligned_picture_size - requested_picture_size)` **after the check**.
That later increase can exceed the checked limit. Actual `AMD_DEBUG=ib`
command traces matched this explanation:

| Requested picture | Aligned height | Padding height sent | Result |
| --- | --- | --- | --- |
| 1280x833 | 848 | 15 | VCN reset |
| 1280x834 | 848 | 14 | Passed |

Other controls passed: 1280x835 and 960x637 without manual padding; and
1280x833 GPU-padded to 1280x834, including independent libdav1d decoding of
all 60 frames. That decoder reported 1280x848: the hardware bitstream may
contain further internal padding, so visible cropping remains necessary.

For pre-VCN5, the ordinary picture alignment is 64x16 and the checked padding
limits are 62x14. Even dimensions satisfy those limits for every remainder.
The special `height % 16 == 8` branch uses `height + 2`, also within the limit.
The VCN5 branch uses 8x2 with padding limits 6x0; even dimensions satisfy those
too. The rule is thus broader than special-casing height 833, but is **not** a
claim that AV1 forbids odd dimensions. FFmpeg distinguishes visible picture
dimensions from aligned reconstruction storage in its
[AV1 VA-API encoder](https://github.com/FFmpeg/FFmpeg/blob/n8.1.2/libavcodec/vaapi_encode_av1.c).

This policy is deliberately conservative across VA-API vendors; Weld does not
parse vendor/version strings to decide whether an odd picture is safe. Keeping
the small pad is preferable to prematurely assuming a newer driver is fixed.
It does not promise protection from every GPU/firmware defect.

### Upstream status and removal criteria

Checked on 2026-09-12: the padding-check ordering remains in Mesa 26.2.2 and
[upstream source at abe4706e](https://gitlab.freedesktop.org/mesa/mesa/-/blob/abe4706e0fa4a7a9f9afffad077eefb94c186d6c/src/gallium/drivers/radeonsi/mm/radeon_vcn_enc.c).
No matching public fix was identified. Command traces strongly implicate the
out-of-bound padding, but no patched-Mesa A/B run isolated firmware causality.
Do not remove the policy just because a release mentions an AV1 fix.
Removal/narrowing needs a relevant upstream change and odd-size encode/decode
validation on the affected driver/firmware combinations, including resizing.

Related upstream reports are **not** interchangeable with this reproduction:

- [Mesa issue 9185](https://gitlab.freedesktop.org/mesa/mesa/-/work_items/9185)
  documents AV1 output padding on AMD; it does not establish this reset's cause.
- [MR 44077](https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/44077),
  included in [Mesa 26.2.2](https://docs.mesa3d.org/relnotes/26.2.2.html), fixes
  `allowed_max_bitstream_size` being zero. Both the passing and failing traces
  had zero. This is a real additional driver defect, but an updated-driver run
  is needed to determine its interaction with this reset. Weld cannot repair
  Mesa's internal command field through the current FFmpeg interface.
- Open [MR 44080](https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/44080)
  disables 4x pre-encoding for non-4x4-aligned pictures. Our failing command
  explicitly had preencode mode `NONE`; this is not its active path.

## Existing AV1 minimum-size fallback

`query_encode_geometry` respects queried minimum/maximum attributes. If the
driver omits AV1 minimum dimensions, Weld supplies a conservative 128x128
fallback. A 192x64 popup previously failed encoder startup with a reported
128-pixel minimum height. The same pad/crop path allows such small windows
without changing their visible size. Explicit driver minima take precedence;
128x128 is not an AV1-wide rule. Prefer complete, validated capability data
before relaxing this fallback on other devices.

## Existing AV1 bitrate ceiling

`VaapiEncoderSettings::try_new` rejects AV1 targets above 8,000,000 bits/s.
An earlier 64 Mbps Radeon VCN probe lost its GPU context. This is a conservative
validated operating point, not a universal AV1 maximum, and the size-triggered
reset reproduced below it at 7,552,000 bits/s. It is neither a hard network
bandwidth cap nor proof that all lower-rate configurations are safe.
Lifting the ceiling requires separate driver-qualified bitrate testing; no
upstream fix for that earlier experiment has been established here.

## Validation

CPU tests cover the exact failing size, odd widths, minimum-before-alignment,
post-alignment maximum rejection, overflow, unchanged H.264 geometry, and a
range of alignment remainders. The diagnostic round-trip probe recreates AV1
contexts for 1280x833, 960x637, and finally 1281x833; it checks keyframes,
timestamps, original visible output dimensions and grayscale corner samples.
Odd-width coverage extends beyond the original measured height-only failure.
The original probes keep their 16-level pixel tolerance. New odd-size probes
keep that bound on non-padded edges and inset samples, but use a named 32-level
bound at visible pixels immediately adjacent to right/bottom padding. All
original corner positions are still checked; raw edge samples are also printed.

The first 1281x833 probe failed the original 16-level corner bound: a source
gray value of 149 decoded as 126 at the bottom-right corner on the keyframe
(23 total error; the next two frames were within 5 levels),
while a one-pixel-inset sample was 147 to 140 and the center was 85 to 76.
The approximately 7-9-level interior bias is separate from the approximately
15-level additional edge deviation; do not attribute the whole 23 to padding.
Both the gray source and black pad are chroma-neutral, so chroma subsampling
does not explain this measured luma error. Codec in-loop filtering at the sharp
pad boundary is one plausible explanation, but encoder, NV12 conversion and VPP
stages have not been isolated. The wider, edge-only assertion records this
accepted quality limitation while still detecting substantially worse output.
It does not establish pixel-perfect edges or validate chroma fidelity.

```sh
cargo test -p weld-media-vaapi --lib -j2
cargo test -p weld-hoist-encoded --features vaapi --lib -j2
cargo build -p weld-media-vaapi --features diagnostic --example vaapi_roundtrip_probe -j2
ulimit -c 0
timeout --signal=TERM --kill-after=3s 30s cargo run --quiet \
  -p weld-media-vaapi --features diagnostic --example vaapi_roundtrip_probe
scripts/run-headless-iroh-hoist --codec av1 --app foot --seconds 20
```

Hardware tests require a compatible development shell and real render-node
access. Even bounded tests can reset a faulty GPU and affect other processes;
do not run them as unattended CI. These tests write no video dumps. A timeout
is not proof that a GPU workload was cancelled. The headless launcher owns
and cleans up its test processes and disables core dumps.

### Workaround validation on 2026-09-12

- CPU: 12 VA-API tests and 126 encoded-hoist tests passed; VA-API Clippy with
  all targets, diagnostic feature, and `-D warnings` passed.
- Production GPU round trips passed the existing H.264/AV1 depth-1/depth-2
  checks, 192x64 AV1 popup crop, and 16 encoder-generation descriptor-lifetime
  check. New 1280x833, 960x637 and 1281x833 cases each passed three encode/decode
  frames with visible-crop and the explicit grayscale bounds described above.
  The final run is retained at `target/validation/av1-even-geometry-probe.log`.
- `headless-iroh-dyxuahev`: AV1 foot/htop, 20-second timer, clean shutdown.
- `headless-iroh-koyp09cw`: AV1 Blender, 20-second timer, startup resize from
  1920x1080 to 2400x1350, clean shutdown. Both runs have source/destination logs
  under `target/validation`; no new kernel VCN timeout/reset was recorded.
- `headless-iroh-3jqyubdv`: AV1 foot/htop and Blender together, two active
  encoder sessions, 20-second timer and clean shutdown, with no new VCN reset.

These are bounded smoke tests, not proof of stability across all applications,
window sizes, bitrates, drivers or extended sessions.
