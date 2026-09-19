# Receiver efficiency investigation

Read-only audit on 2026-09-19, starting from `2f94ab09`. The findings below
describe the Android/Godot live receiver, not an assumption about every provider.
Recommendations are experiments, not measured causes of choppiness.

## Current allocation and copy boundaries

The compressed data path has three materialization/copy boundaries: QUIC data
into an owned receive `Vec`, `Packet::copy` into FFmpeg-owned padded storage,
then FFmpeg into a MediaCodec input buffer. Protocol admission and decode jobs
move the owned payload rather than repeatedly cloning it. At 24 Mbps the stream
contains 3 MB/s of compressed data before overhead; this is not equivalent to
moving uncompressed video at display rate. Allocation and scheduling tails can
still matter even when aggregate copy bandwidth is modest.

- `weld-hoist-iroh/src/framing.rs` allocates/zeroes each media payload and uses
  fresh header scratch. Control readers/writers already reuse scratch storage.
- `weld-media-android/src/decoder.rs` repeats packet allocation/copy on each
  unsuccessful submission attempt. Its receive method allocates an AVFrame
  wrapper on every FFmpeg poll, including EAGAIN. The ffmpeg-next AVPacket
  struct itself is inline; its referenced payload allocates.
- `playback/session.rs` republishes an `Arc<Presentation>` and vectors after
  every event, including unchanged topology. `window_frames.rs` rebuilds layout
  and snapshot vectors; input metadata also gets cloned.
- Godot textures/materials and stream-generation decoder contexts are reused.
  They are created lazily, not all preallocated at application startup.
- Decoded pixels remain in native buffers, through ImageReader and EGL import.
  There is no CPU pixel download/upload. Godot still draws the sampled content
  into an XR composition viewport: this is not direct decoder-to-XR scanout.
- Each presented image creates an EGLImage view; retirement exports a release
  fence and destroys the old view. These are native resource operations, not
  new copies of all pixel data. Android manages backing-buffer allocation/reuse.
  ImageReader's `max_images` limits acquired images; it does not promise that
  all graphic buffers were allocated upfront.

Bounded queues and image credits limit retention; they do not eliminate
allocations. Pooling must retain those bounds, including limits on cached bytes,
and should not reserve the maximum permitted packet size for every queue slot.

## Prior art

- [Moonlight Android JNI](https://github.com/moonlight-stream/moonlight-android/blob/master/app/src/main/jni/moonlight-core/callbacks.c)
  reuses a growable staging array. Its [MediaCodec renderer](https://github.com/moonlight-stream/moonlight-android/blob/master/app/src/main/java/com/limelight/binding/video/MediaCodecDecoderRenderer.java)
  copies into codec-owned inputs, separates output draining from input, and
  supports a two-frame Choreographer-paced output queue. Surface output avoids
  our app-owned XR texture-import/composition path.
- [Moonlight desktop's FFmpeg backend](https://github.com/moonlight-stream/moonlight-qt/blob/master/app/streaming/video/ffmpeg.cpp)
  reuses packet/staging storage while still allocating output AVFrame objects.
  It polls for additional input while awaiting output. Mature implementations
  are not universally allocation-free.
- [ALVR's Android decoder](https://github.com/alvr-org/ALVR/blob/master/alvr/client_core/src/video_decoder/android.rs)
  uses ImageReader-backed hardware buffers with an image listener. Its
  [GL import path](https://github.com/alvr-org/ALVR/blob/master/alvr/graphics/src/lib.rs)
  creates/destroys EGLImages during rendering. Per-image import alone is not
  evidence of an incorrect design.
- Steam's internal allocation strategy was not independently verified; no
  implementation claim here relies on inferred proprietary internals.

## Measurements and interpretation

The full saved `godot-hoist-_e0wt5p7` capture ended at 11535 decoded,
11090 imported, 420 superseded and 23 stale frames. Import/bind/retirement CPU
time averaged 0.452 ms per imported frame, with a 1.504 ms interval maximum.
Decoder-worker residence averaged about 6.09 ms; this is wall time including
waiting, not isolated hardware execution. Warm-up and network variation are
included. These counters are neither physical scanouts nor end-to-end latency.

Do not attribute the entire import block to EGLImage creation or predict that
caching removes all of it. Split create/bind/fence/retire timings first.
Likewise, EAGAIN retry frequency and allocation counts have not been measured.

## Candidate work, kept separate

1. Retain a prepared packet through EAGAIN and reuse AVFrame scratch storage.
   Reuse media-header scratch and publish topology only when its metadata or
   lifecycle epochs change. Verify allocation reduction with counters/profiling.
2. Investigate codec output, ImageReader and acquire-fence waits separately.
   The cooperative worker has a 1 ms active-job fallback poll. Pinned FFmpeg can
   enter repeated 8 ms output waits when input buffers are unavailable, including
   through `avcodec_send_packet`; a nonblocking Weld facade does not prevent this.
3. Explore bounded reusable compressed storage. A borrowed AVPacket does not
   remove the copy: FFmpeg copies non-reference-counted input internally.
   True ownership transfer requires FFmpeg padding and reference-safe release.
4. Consider EGLImage caching only with measured benefit. Cache buffer identity
   and owned storage references, not transient AImage pointers. Account for
   [Android buffer-removal notifications](https://developer.android.com/ndk/reference/group/media#aimagereader_setbufferremovedlistener),
   generation/context teardown and bounded retained bytes. A cached import never
   replaces the acquired-image lease or per-use acquire/release fences.

The known Godot native Preferences-close crash remains separate and unresolved.
Do not use an apparently successful launcher exit as sole proof of a successful
Android run; inspect viewer logs, process liveness and Android exit information.

## Standard low-latency experiment

[Android's low-latency key](https://developer.android.com/reference/android/media/MediaFormat#KEY_LOW_LATENCY)
asks supported decoders not to retain data longer than required by the codec.
[Moonlight](https://github.com/moonlight-stream/moonlight-android/blob/master/app/src/main/java/com/limelight/binding/video/MediaCodecHelper.java)
requests it with compatibility handling. Weld's pinned FFmpeg 8.1.2 revision
`38b88335f99e76ed89ff3c93f877fdefce736c13` did not forward this key. Its public
MediaCodec API exposes output buffers/surfaces, not the underlying codec handle.

`scripts/ffmpeg-patches/mediacodec-low-latency.patch` makes video decoder setup
translate `AV_CODEC_FLAG_LOW_DELAY` to `low-latency=1` before configuration.
This follows the mechanism proposed in an [upstream discussion](https://ffmpeg.org/pipermail/ffmpeg-devel/2023-May/310004.html);
the proposal is not assumed to have landed. No vendor key, priority or operating
rate is changed. Patch bytes are part of the Android build cache key, and a
unique binary string verifies patch inclusion without relying on FFmpeg stderr
being visible in Godot logcat. Recheck upstream support before updating FFmpeg.

The opt-in `--decoder-low-latency` flag on `scripts/run-godot-xr` builds and
deploys the experiment. `scripts/run-godot-hoist` accepts the same flag when the
matching patched APK is already installed. Ordinary launches retain baseline
behaviour. A private one-shot marker selects the request for the receiver
session, including reconnects and new decoder generations; it is not a peer
capability or persistent user preference. Desktop use is rejected.

Rust logs `requested_low_latency` at decoder creation. This proves what Weld
requested, not whether hardware honoured it. Compare baseline and requested
mode using the same APK, content, bitrate and cadence; test full and half rate
separately. Configuration errors must be reported rather than silently falling
back and contaminating the comparison. Production capability negotiation and
fallback policy remain future work. Queue depths, frame age and image leases
are unchanged by this experiment.

### Initial Pico result, 2026-09-19

The installed patched APK played AV1 successfully in both modes. The metadata
probe (`run-android-codec-probe --codec-info c2.qti.av1.decoder`) reported
hardware=true, software=false, vendor=true and **low_latency_feature=false**.
This codec does not advertise the standard feature. The query does not establish
whether it ignores the request or changes undocumented internal behaviour.

Four 40-second runs used the same APK, 16 Mbps target and `blender_motion.py`.
The single-window column below weights worker residence by completed timing
samples between receiver elapsed seconds 8 and 20; the second column uses
seconds 28 through 35 with Preferences open. These are wall-time measurements,
not hardware execution times or a statistically established effect.

| Source cadence / request | Single-window mean | Preferences-open mean | Run suffix |
| --- | --- | --- | --- |
| 90 fps / baseline | 5.621 ms | 6.150 ms | `iu5i5zgo` |
| 90 fps / low latency | 5.681 ms | 6.406 ms | `e26gz9lp` |
| 45 fps / baseline | 5.867 ms | 6.257 ms | `viz3g034` |
| 45 fps / low latency | 6.804 ms | 6.362 ms | `n52y4xp1` |

All four reported zero codec failures and zero QUIC packets declared lost, but
RTT and frame replacement counts varied. Android exit information confirmed
normal test force-stop for each. The runs ended before Preferences closed:
the preceding unmodified 75-second baseline (`4gca8wtq`) reproduced the known
Godot native crash at that operation. This experiment does not fix that crash.

There is **no demonstrated latency benefit** on this Pico AV1 decoder, so the
request remains opt-in. Testing another codec/device or vendor options is a
separate experiment, not a reason to silently change this comparison.

Verification: 70 Godot Rust tests, strict Linux/Android Clippy, 13 hoist-launcher
tests, 7 XR-launcher tests, 8 diagnostics tests, FFmpeg patch dry-run/build,
Linux/Android extension builds, Pico export and scene smoke passed. The packaged
`libavcodec.so` matched the staged patched library (SHA-256
`7be4d695dd92ebcb59a2fc8cdb5001b00b375a1c6b7321f46156cfb6b0278525`). Godot strips
the extension during export; packaged/staged `.text` hashes matched, and both
contained the experimental mode markers. Generated binaries and logs stay
untracked; baseline FFmpeg cache was preserved.

## H.264 comparison, 2026-09-19

The live Godot receiver now accepts AV1 and H.264. Android's FFmpeg MediaCodec
H.264 wrapper needs SPS/PPS extradata when opening the decoder: pinned FFmpeg's
`ff_h264_decode_extradata` rejects an empty input. The Android provider extracts
bounded Annex B parameter sets from the first access unit when configuration
extradata is absent. Linux keeps its existing in-band header handling. Decoder
generations, scheduling, frame queues and image leases are unchanged.

Four 40-second full-rate Blender motion runs used an 8 Mbps shared target,
with the low-latency request disabled. This avoids an unequal comparison caused
by AV1's existing per-stream bitrate cap. The first AV1 run predates the H.264
header fix; the second pair used the same APK, and the AV1 path was unchanged.

| Codec | Single-window worker mean (seconds 8–20) | Run suffix |
| --- | --- | --- |
| AV1 | 5.510 ms | `n_bot80v` |
| H.264 | 5.132 ms | `o681ogfd` |
| AV1 | 5.597 ms | `1umi7kt7` |
| H.264 | 5.717 ms | `_mup1u4a` |

Worker residence includes waiting, not just hardware decoding. H.264 encoding
was slightly faster in these samples, but receiver timings and replacements
varied enough that there is no demonstrated overall latency win. Both codecs
reported zero codec errors and QUIC packets declared lost. Tests ended before
the known Preferences-close crash. The Pico used `c2.qti.avc.decoder`, which
also reported `low_latency_feature=false`.

Subsequent manual testing found no visible improvement with H.264 and possibly
worse appearance. AV1 therefore remains the default; H.264 stays available for
comparison and other devices. These observations are not a general codec
quality ranking or an end-to-end latency measurement.
