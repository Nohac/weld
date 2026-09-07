# Local cross-process hoisting

Weld has a same-machine validation transport for hoisting client surfaces
between two sibling compositor processes. It exists to exercise the same
client-adapter, input, configure, scale, reclaim, and buffer-lifetime contracts
that a later network/codec transport must implement.

## Runtime boundary

The source remains the authoritative Wayland compositor for the real client.
The destination registers a Relocated `weld-client` adapter and presents the
transported toplevels and popups through ordinary window admission.

`weld-hoist-core` supplies the shared source and destination relays used by
both this binding and same-process loopback. Those relays own session checks,
surface caching and replay, popup recursion, identity relocation, request and
input back-routing, scale reset, and input cleanup on peer loss.
`weld-hoist-local` supplies only binding mechanics: Postcard records, Unix
packet and descriptor ownership, native buffer import/export, encoded-media
state, bounded queues, and calloop wake sources. A local source or destination
failure therefore enters the same hoist cleanup policy as another transport.

Native mode has these properties:

- Unix `SOCK_SEQPACKET` preserves one Postcard record per message.
- Linux `SCM_RIGHTS` carries native-buffer descriptors beside the commit that
  describes them.
- Both endpoints verify `SO_PEERCRED` against Weld's effective UID and validate
  the opposite Source/Destination role on every packet.
- Destination control packets must not carry file descriptors. Receiving one
  is fatal because skipping it would discard descriptor ownership and could
  desynchronize descriptor indices in later packets.
- One stable epoll descriptor wakes Smithay's calloop. Writable readiness is
  enabled only while a send queue is nonempty.
- The first committed use imports an allocation. Later uses name the stable
  buffer without resending its descriptors. Buffer destruction or final
  session unmap retires the destination import.
- Per-commit release remains separate from allocation retirement. Source
  Wayland release occurs only after the destination's final renderer lease is
  dropped.
- DMA-BUF content keeps the bind-once descriptor path and does not copy pixels.
  A client-provided SHM buffer uses a deliberately separate compatibility path:
  its already-normalized packed BGRA pixels are copied through a sealed
  anonymous file descriptor and validated at the destination. SHM never
  enters the reusable DMA-BUF import cache.
- The outbound packet and descriptor queue is bounded. A peer that stops
  draining fails the local session cleanly instead of retaining unbounded
  per-frame SHM descriptors.

The optional `encoded-opaque` mode keeps the same control and input protocol
while replacing native surface-buffer transfer with an H.264 or AV1 hardware
codec path:

- The source selects the mode during a request/acknowledgement bootstrap. It
  passes one end of a fresh seqpacket socketpair to the destination with
  `SCM_RIGHTS`; media therefore cannot fill the independent control queue.
- Both sides exchange and require the exact current pre-1.0 hoist protocol
  revision before accepting the selected surface mode.
- One capacity-one worker per endpoint owns persistent FFmpeg VA-API sessions
  for the bootstrap-selected codec. Worker completion wakes calloop through
  eventfd rather than polling.
- DMA-BUF input remains device resident and its source lease completes after
  VPP and encoding. Foot's already-normalized SHM pixels are copied once into
  owned worker input and release immediately after that copy.
- Encoded access units are carried in bounded sealed descriptors. The
  destination decodes and VPP-converts them into XRGB DMA-BUFs at the exact
  visible client extent; those imports retire after destination GPU use.
- Resize commits coalesce while the destination reports an active resize. The
  last decoded frame scales with the Weld window, and the newest settled
  client commit starts the replacement codec generation.
- Encoded commits require no application acknowledgement. Local send capacity
  admits new encode batches; pressure coalesces superseded unencoded commits.
  Completed media retains codec order, while control has its own FIFO.
  Independently bounded receiver admission pauses reads until decoding frees
  room. Decoder generation retirement follows remaining references rather than
  arrival of later resize metadata. These bounds are not latency guarantees;
  see [ACK-free streaming](ack-free-streaming-plan.md).
- Destination control packets flush immediately through the nonblocking Unix
  socket when it is writable. `EAGAIN` leaves them in a bounded 256-packet
  userspace queue for calloop; reaching that bound remains a fatal stalled-peer
  condition and reports whether the attempted preflush was blocked.
- This tracer intentionally discards alpha. Each replaced surface-tree layer
  has its own persistent codec stream. Multi-layer commits are encoded
  sequentially through the capacity-one worker and become visible only after
  every layer has decoded, preserving the source commit's atomicity at the
  cost of one codec round trip per replaced layer. A commit is limited to 16
  replacements and the worker retains at most 16 layer streams until
  capability-driven media budgeting replaces this tracer policy.
- Unsupported hardware, y-inverted input, or a commit beyond those explicit
  bounds fails the session; there is no silent native or software fallback.

H.264 currently uses constrained baseline at 16 Mbps. AV1 uses Main at 8 Mbps;
the settings type rejects higher AV1 bitrates because a supervised 64 Mbps
radeonsi probe reset the VCN context. Both paths use a microsecond time base,
one-packet-per-frame low-delay encoding, and the exact visible extent carried
by the client protocol. The bitrate applies to each current surface-tree layer
stream, not once across an entire hoisted top-level; composition and shared
budgeting remain future work. In particular, AV1 may decode into 960x496 coded
storage for a 944x484 client surface; destination VPP crops that storage rather
than allowing coded padding to affect window geometry. The backend queries the
selected VA profile and entrypoint's minimum and maximum surface geometry. If
an AV1 driver omits its minima, Weld uses the validated 128x128 fallback. On
the Radeon 880M, a 192x64 popup is therefore encoded in 192x128 storage while
retaining 192x64 as its authoritative visible geometry.

The source compositor still completes Wayland frame callbacks according to its
own mapped-surface presentation cadence. A transported surface should
eventually inherit demand and cadence from its destination instead; source
local backpressure prevents that mismatch from becoming unbounded media work but
does not yet stop the hidden source client from rendering coalesced frames.

An August 31, 2026 Blender input-stress trace on the Radeon 880M validated the
credit stopgap for 55 seconds without queue failure: 1,591 one-layer commits
were decoded and applied while 1,795 newer commits coalesced at the source.
Median/p95 hardware decode completion was 6.4/10.4 ms (15.1 ms maximum), and
median/p95 destination-credit round trip was 8.7/15.2 ms. That run rules out
multi-layer multiplication and insufficient decoder throughput as the primary
cause of its visible smearing. The fixed 16 Mbps, NV12 4:2:0 H.264 quality and
reference policy remains the next diagnostic target. A manual standard versus
QP-16..28 comparison produced virtually identical average payloads (33,486 and
33,475 bytes), so CBR probably kept the operating QP inside both ranges. The
visual difference cannot be attributed to the tighter QP bounds. `high-bitrate`
now changes only CBR from 16 to 64 Mbps, while corrected `all-idr` uses the
standard QP 18..36 range and changes only reference/session behavior as closely
as current cros-codecs permits. Independent mode recreates the encoder session
for every frame because current cros-codecs cannot otherwise emit a conforming
IDR with SPS/PPS. That resets rate control, so its timing and bitrate are not a
controlled comparison.

Weld locally patches cros-codecs to size VA coded buffers from the coded extent
(`3 * width * height + 64 KiB`) instead of treating bits per second as a byte
allocation. This follows [FFmpeg's VA encoder](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/vaapi_encode.c)
and avoids a 128 MB buffer allocation for every 64 Mbps frame. The patch also
rejects VA overflow and bad-bitstream status and traces the driver-reported
average QP. Remove it when upstream cros-codecs provides a resolution-bounded
coded buffer and equivalent status handling.

[FFmpeg explicitly queries and supplies VA encoder quality levels](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/vaapi_encode.c),
which cros-codecs does not yet do. Mesa 26.1 also contains several
[radeonsi/VCN encode-preset fixes](https://docs.mesa3d.org/relnotes/26.1.0.html).
Explicit quality-level support is the next isolated encoder diagnostic if
bitrate is not binding; it is not mixed into this comparison. Automatic
per-device round-trip validation remains deferred to the capability-negotiation
pass, where peers should advertise only profiles that survive validation.

### Independent GStreamer encoder probe

`scripts/run-gstreamer-vaapi-probe` is an isolated diagnostic comparison, not a
Weld runtime dependency or a production codec decision. Its standalone Cargo
workspace feeds the same deterministic XRGB DMA-BUF frames to Weld's patched
cros-codecs H.264 path and to GStreamer's VA-API H.264 path. The runner rejects
system-memory negotiation, software-decodes both streams, checks range anchors,
SPS geometry and profile, measures PSNR, and records the executable, plugin,
linkage, and Nix closure footprint. Run it with:

```sh
scripts/run-gstreamer-vaapi-probe
scripts/run-gstreamer-vaapi-probe --render-node /dev/dri/renderD128
```

Each run writes a timestamped directory under
`target/validation/gstreamer-vaapi-probe-*`. The test source contains stable
black, white, and mid-gray patches, localized 8-pixel detail, and moving
high-contrast regions. The stable patches distinguish range conversion from a
transfer-curve conversion instead of relying on PSNR alone. All pixels are
grayscale, so this probe intentionally does not validate RGB channel order.

A September 1, 2026 Radeon 880M run established GStreamer integration
viability without establishing that GStreamer fixes Weld's Blender artifact.
GStreamer negotiated modifier-bearing XRGB DMA-BUF input, `VAMemory` NV12
postprocessing, and constrained-baseline H.264 output. It emitted all 64 frames
with the expected 944x484 display and 944x496 coded geometry. The SPS reported
`profile_idc=66`, constrained-baseline flag 1, 59 macroblocks horizontally, 31
vertically, and a 12-row bottom crop. Stable luma anchors decoded to 16, 235,
and 126, confirming limited range while preserving the requested sRGB transfer.
Its range-normalized PSNR was 46.09 dB average and 39.99 dB minimum.

The cros-codecs arm had matching geometry, cadence, and constrained-baseline
profile, but its stable anchors decoded to 3, 241, and 125. The endpoints match
neither standard full nor limited range and are an independent cros-path
anomaly. Because that range cannot be classified, relative PSNR is deliberately
skipped. None of the planned clean-versus-corrupt A/B interpretation branches
was reached, and the large Blender corruption was not reproduced by either arm.
Both arms nevertheless preserved mid-gray within one code value. Matching
mid-tones with divergent endpoints narrows the cros anomaly to endpoint or
clamping behavior, not a transfer-curve or matrix conversion. The result
narrows future work to the real client-buffer input and rate-control conditions
rather than proving that a codec framework replacement solves it.

The same run also supports keeping GStreamer out of Weld's production graph.
The diagnostic executable was 49 MB with debug information and dynamically
loaded a 650 KB VA plugin. More importantly, Nix reported overlapping closure
sizes of roughly 244 MB for GStreamer core, 320 MB for plugins-base, and 893 MB
for plugins-bad. These closure figures are not additive, but they demonstrate a
substantial runtime and packaging commitment compared with a narrow codec
adapter. GStreamer remains useful as an independent implementation oracle;
FFmpeg, Vulkan Video, and narrower native backends remain production candidates.

### Independent FFmpeg encoder probe

`scripts/run-ffmpeg-vaapi-probe` is a second standalone diagnostic. It uses
`ffmpeg-next` for version and linkage integration and keeps the unavoidable
DRM PRIME, hardware-frame, filter-graph, and encoder calls behind one small raw
FFmpeg boundary. The probe wraps owned XRGB DMA-BUFs as
`AV_PIX_FMT_DRM_PRIME`, derives one VA-API device per worker from its DRM
device, supplies it to `hwmap`, performs the RGB to limited-range NV12
conversion with `scale_vaapi`, and feeds those hardware
frames directly to the selected `av1_vaapi` or `h264_vaapi` encoder. It does
not map the source or normalized frame into CPU memory. Run it with:

```sh
scripts/run-ffmpeg-vaapi-probe
scripts/run-ffmpeg-vaapi-probe --render-node /dev/dri/renderD128
scripts/run-ffmpeg-vaapi-probe --codec h264
scripts/run-ffmpeg-vaapi-probe --codec h264 --bitrate-mbps 8
```

The default is the validated 8 Mbps AV1 path. H.264 defaults to Weld's 16 Mbps
production setting. The runner refuses other AV1 bitrates unless
`WELD_FFMPEG_ALLOW_UNSAFE_AV1_BITRATE=1` is set because the first 64 Mbps AV1
experiment caused radeonsi to declare the VCN context guilty and perform a hard
GPU recovery after frame 16. This override is for supervised driver diagnosis,
not normal validation.

Each run writes a timestamped directory under
`target/validation/ffmpeg-vaapi-probe-*`. The runner requires FFmpeg's debug
trace to show a DRM object mapped into VA-API, verifies codec-specific profile
and geometry, software-decodes the result, checks stable range anchors,
measures PSNR, and records dynamic linkage and binary size. The initial upload
of the deterministic test pattern is intentionally CPU-side; the boundary
being validated starts at the resulting DMA-BUF.

The encoder and filter graph use microsecond timestamps. The diagnostic IVF
carrier translates them back to sequential 60 Hz frame indices because IVF's
header declares a 1/60-second time base. Its image decode is hard-limited to 64
frames with timestamp passthrough and verifies exactly 64 output files. Keep
those bounds: mixing microsecond packet timestamps with the old IVF time base
caused FFmpeg to synthesize a multi-hour image sequence during development.

A September 1, 2026 Radeon 880M run using the production one-second CBR
reservoir
imported all 64 explicit-linear XR24 DMA-BUFs and emitted 64 H.264 packets.
FFmpeg reported direct DRM-to-VA-API mapping, VA-API HQ scaling into NV12, CBR
at 16 Mbps, and the constrained baseline hardware profile. The stream decoded
to the expected 944x484 display extent from a 944x496 coded extent. Its stable
luma anchors were 16, 235, and 126, and range-normalized PSNR was 51.23 dB
average and 43.44 dB minimum. It did not reproduce the displaced macroblock
bands seen in Blender captures.

These results selected FFmpeg for Weld's initial production hardware codec
backend. The independent probe still uses linear generated buffers and remains
useful for separating encoder quality from real client modifiers, explicit
synchronization, surface-tree composition, and transport behavior. The backend
is dynamically linked to FFmpeg's `libavcodec`, `libavfilter`, `libavformat`,
and `libavutil`.

The same production-reservoir probe validates AV1 through `av1_vaapi` without
changing the DMA-BUF import or VA-API scaling stages. At 8 Mbps it emitted and
decoded all 64 frames, preserved the 16, 235, and 126 range anchors, and
measured 51.03 dB average and 45.92 dB minimum PSNR. These deterministic
results validate both production operating points; their different bitrate
budgets do not establish a general codec-quality comparison for desktop
content.

A September 1 Blender stress run with an experimental four-frame CBR reservoir
timed out the Radeon `vcn_unified_0` ring before the destination control queue
filled. The kernel reset the ring, and Mesa then called `abort` while FFmpeg
destroyed the guilty VA encoder context during hoist teardown. Restoring the
previously validated one-second reservoir removes that experiment, but one
clean stress run cannot prove it caused the hardware fault. Codec work remains
in-process for this tracer; process-isolated codec workers are the robust
future boundary for containing a driver abort without taking down the
compositor.

On this radeonsi path, AV1 expands the 944x484 input into a 960x496 bitstream
and reports no smaller AV1 render rectangle. FFmpeg's ordinary software-upload
VA-API command produces the same expansion, so it is not caused by Weld's
DMA-BUF wrapper or IVF carrier. The probe crops the decoded image back to the
separately known 944x484 surface extent before checking samples and PSNR. Weld's
transport already needs authoritative visible geometry, and a production AV1
decoder must apply it rather than trusting the coded extent.

The same Radeon exposes VP9 Profile 0 and Profile 2 hardware decoding but no
VP9 VA-API encoding entrypoint. FFmpeg contains `vp9_vaapi`, but that encoder
cannot operate on this device. VP9 therefore remains a negotiated backend for
hardware that actually advertises encoding support; it is not included in this
machine's probe modes.

The August 31 artifact investigation also exposed a visible-versus-coded extent
contract hidden by the old cros-codecs H.264 API. A 944x484 Blender surface
declared a 944x496 coded picture while the driver received only a 944x484
surface, matching the displaced macroblock bands in the recorded bitstream.
The FFmpeg backend owns its aligned VA surface allocation and SPS crop together,
so Weld supplies only the exact visible source extent. Stream generations still
rotate when that extent changes, and the destination always crops decoded coded
storage back to transported visible geometry. The same rule handles H.264 odd
chroma extents and AV1's larger 960x496 allocation without exposing padding in
the window.

Destination input is already addressed to the transported surface and enters
the source's client runtime without the destination's compositor-global
coordinates. Configure, focus, close, and preferred-scale requests re-enter the
normal source adapter. On disconnect, Weld releases every remotely held key,
button, gesture, and finger-scroll sequence before restoring the source.

## Run the validation pair

From a graphical development shell, run:

```sh
scripts/run-local-hoist
```

The script starts two nested sibling Weld instances with distinct Wayland
sockets. The source launches Foot. Focus Foot in the source window and press
`Super+H`; the source should retain a Reclaim placeholder and the live client
should appear in the destination window.

The script defaults to hardware H.264 in `encoded-opaque` mode. Select AV1, use
the native descriptor baseline, or choose another source client with:

```sh
scripts/run-local-hoist --native
scripts/run-local-hoist --codec av1
scripts/run-local-hoist firefox
scripts/run-local-hoist --trace-media blender
scripts/run-local-hoist --codec av1 --trace-media blender
scripts/run-local-hoist --codec h264 --dump-encoded blender
```

`--trace-media` keeps ordinary dependencies at `info` while enabling structured
source-batch, destination-decode, coalescing, and local queue timing for the
encoded boundary. For example, 60 surface-tree commits
per second with two changed
layers requests 120 sequential codec frames per second in the current tracer;
the trace records both the commit and layer counts so that multiplication is
visible separately from per-frame decode time.

`--dump-encoded` creates a timestamped directory printed by the script and
records each source stream generation independently. H.264 uses `.h264`; AV1
uses concatenated low-overhead `.obu`. This keeps evidence from earlier runs
out of the current comparison. Inspect the relevant Blender stream with
software decoding so the destination VA decoder is not reused:

```sh
ffplay -hwaccel none -f h264 PRINTED_DUMP_DIRECTORY/stream-1-generation-1.h264
ffplay -hwaccel none -f obu PRINTED_DUMP_DIRECTORY/stream-1-generation-1.obu
```

Use the filename emitted by the source log; Blender can advance generations
when its extent changes. Ghosting in this file places the defect before local
transport, in source-buffer readiness, VPP, or encoding. A clean file places
it after transport, in destination decoding, VPP, DMA-BUF import, or final
composition.

The same directory contains sampled source snapshots under `stages/`. Through
sequence 300, every thirtieth encoded frame writes a
`stream-N-generation-N-sequence-N-source.ppm` file. FFmpeg owns the normalized
NV12 hardware frame, so Weld no longer duplicates that stage solely for a
diagnostic image. Stage capture performs synchronous VPP work and file writes
in the encoder worker, so timing traces from dump runs must not be
used as ordinary performance measurements.

The eventual one-frame-per-surface-tree path must not use a repacked atlas on
every commit. It requires sticky macroblock-aligned slots with gutters,
bucketed extents, area-aware quality budgeting, a nonfatal codec-ceiling
policy, shared decoded-image lifetime in Bevy, and both batched and sequential
same-target VA-API composition. That work is architectural and is not assumed
to fix the current H.264 smearing by itself.

Validate the following before treating a change to this boundary as complete:

1. Type, move the pointer, click, and scroll in the destination presentation.
2. Resize the destination repeatedly; client content should settle without
   retained file-descriptor or imported-image growth.
3. Move the destination presentation between differently scaled outputs. The
   real client should follow destination scale while the source placeholder's
   output does not control it.
4. Open client-owned popups and related toplevels; they should follow the
   transported owner and preserve their roles.
5. Reclaim from the source. The client must first commit the placeholder-sized
   configure, then remap into its preserved source slot.
6. Hoist again, hold input in the destination, and terminate the destination.
   The source must restore without a stuck key, pointer grab, gesture, focus,
   or compositor exit.

Logs are written to:

- `target/validation/weld-hoist-source.log`
- `target/validation/weld-hoist-destination.log`

The equivalent distribution flags are `--hoist-listen PATH` on the source and
`--hoist-connect PATH` on the destination. `--wayland-socket NAME` allows
multiple Weld instances in one runtime directory.

## Current constraints

- Exactly one peer is admitted during startup; there is no live listener,
  reconnect, discovery, or multi-peer policy yet.
- Both processes must run as the same UID. Native DMA-BUF clients require GPUs
  capable of importing the advertised format/modifier pair; native SHM uses
  the explicit CPU-copy path. Encoded mode currently requires a complete
  VA-API H.264 encode/decode and VPP path on both endpoints.
- The supported topology is sibling compositors. Do not launch the destination
  inside the source's Wayland session.
- The Postcard records are intentionally pre-1.0 and require matching Weld
  builds. The bootstrap rejects a different exact protocol revision; no
  backward-compatibility negotiation is attempted before 1.0.
- Encoded mode supports opaque H.264 and AV1. It has no VP9, alpha plane,
  software codec fallback, cross-device capability negotiation, or
  decoded-output pool.
- Decoder generation retirement is queued when transported layers rotate or
  disappear and is applied by the worker before its next decode command; an
  idle worker can therefore retain the last retired VA session until new work
  arrives.
- There is no network transport, clipboard/DnD transfer, or filesystem path
  mediation in this binding.
