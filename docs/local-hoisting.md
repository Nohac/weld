# Local cross-process hoisting

Weld has a same-machine validation transport for hoisting client surfaces
between two sibling compositor processes. It exists to exercise the same
client-adapter, input, configure, scale, reclaim, and buffer-lifetime contracts
that a later network/codec transport must implement.

## Runtime boundary

The source remains the authoritative Wayland compositor for the real client.
The destination registers a Relocated `weld-client` adapter and presents the
transported toplevels and popups through ordinary window admission.

Native mode has these properties:

- Unix `SOCK_SEQPACKET` preserves one Postcard record per message.
- Linux `SCM_RIGHTS` carries native-buffer descriptors beside the commit that
  describes them.
- Both endpoints verify `SO_PEERCRED` against Weld's effective UID and validate
  the opposite Source/Destination role on every packet.
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

The optional `encoded-h264-opaque` mode keeps the same control and input
protocol while replacing native surface-buffer transfer with a hardware codec
path:

- The source selects the mode during a request/acknowledgement bootstrap. It
  passes one end of a fresh seqpacket socketpair to the destination with
  `SCM_RIGHTS`; media therefore cannot fill the independent control queue.
- One capacity-one worker per endpoint owns persistent VA-API H.264 sessions.
  Worker completion wakes calloop through eventfd rather than polling.
- DMA-BUF input remains device resident and its source lease completes after
  VPP and encoding. Foot's already-normalized SHM pixels are copied once into
  owned worker input and release immediately after that copy.
- Encoded access units are carried in bounded sealed descriptors. The
  destination decodes and VPP-converts them into XRGB DMA-BUFs at the exact
  visible client extent; those imports retire after destination GPU use.
- Resize commits coalesce while the destination reports an active resize. The
  last decoded frame scales with the Weld window, and the newest settled
  client commit starts the replacement codec generation.
- The destination returns one credit after an atomic encoded commit enters its
  `ClientEventQueue`. Each surface may have one unacknowledged commit; newer
  commits coalesce at the source while raw input continues immediately. This
  bounds decode backlog and favors current pixels over catch-up latency, but a
  one-credit window can reduce frame rate when a complete encode/decode round
  trip exceeds the frame interval. Credit does not acknowledge final renderer
  presentation or extend decoded-buffer lifetime.
- This tracer intentionally discards alpha. Each replaced surface-tree layer
  has its own persistent codec stream. Multi-layer commits are encoded
  sequentially through the capacity-one worker and become visible only after
  every layer has decoded, preserving the source commit's atomicity at the
  cost of one codec round trip per replaced layer. A commit is limited to 16
  replacements and the worker retains at most 16 layer streams until
  capability-driven media budgeting replaces this tracer policy.
- Unsupported hardware, y-inverted input, or a commit beyond those explicit
  bounds fails the session; there is no silent native or software fallback.

The source compositor still completes Wayland frame callbacks according to its
own mapped-surface presentation cadence. A transported surface should
eventually inherit demand and cadence from its destination instead; source
credit currently prevents that mismatch from becoming unbounded media work but
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

The August 31 artifact investigation also found a separate visible-versus-coded
extent contract hidden by the cros-codecs H.264 API. Its SPS builder rounds a
visible extent to 16x16 macroblocks and records the crop, while `new_vaapi`
accepts an independent coded size used for VA surface allocation. The API docs
do not state that the latter must contain the rounded SPS extent, and Weld had
passed the visible extent to both. A 944x484 Blender surface therefore declared
a 944x496 coded picture while the driver received only a 944x484 surface, which
matches the displaced horizontal bands in the recorded source bitstream.

`weld-media-vaapi` now owns this adaptation explicitly. The H.264 encoder is
configured with the exact visible extent, its VA surfaces are rounded up to
16x16 macroblocks, and VPP copies the visible source one-to-one into the
top-left of an opaque-black padded NV12 surface without scaling. Stream
generations rotate on exact visible extent changes so SPS crop metadata cannot
drift from a reused encoder session. On radeonsi, black padding affects a few
pixels at the bottom or right edge through in-loop deblocking. Replicated edge
pixels remain a future quality improvement; this narrow border defect is
separate from the large stale macroblock bands under investigation.

H.264 4:2:0 cropping is expressed in two-pixel chroma units. An odd transported
extent therefore decodes to the next even display extent; Weld crops that
decoded surface back to the exact transported width and height instead of
resampling it. The extra coded pixel never becomes part of the destination
window geometry.

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

The script defaults to the hardware `encoded-h264-opaque` path. Use the native
descriptor baseline or choose another source client with:

```sh
scripts/run-local-hoist --native
scripts/run-local-hoist firefox
scripts/run-local-hoist --trace-media blender
scripts/run-local-hoist --trace-media --high-bitrate blender
scripts/run-local-hoist --trace-media --all-idr blender
scripts/run-local-hoist --trace-media --high-bitrate --dump-h264 blender
```

`--trace-media` keeps ordinary dependencies at `info` while enabling structured
source-batch, destination-decode, coalescing, credit timing, VA segment status,
and average QP for the encoded boundary. For example, 60 surface-tree commits
per second with two changed
layers requests 120 sequential codec frames per second in the current tracer;
the trace records both the commit and layer counts so that multiplication is
visible separately from per-frame decode time.

`--dump-h264` creates a timestamped directory printed by the script and records
each source stream generation independently. This keeps evidence from earlier
runs out of the current comparison. Inspect the relevant Blender stream with
software decoding so the destination VA decoder is not reused:

```sh
ffplay -hwaccel none -f h264 PRINTED_DUMP_DIRECTORY/stream-1-generation-1.h264
```

Use the filename emitted by the source log; Blender can advance generations
when its extent changes. Ghosting in this file places the defect before local
transport, in source-buffer readiness, VPP, or encoding. A clean file places
it after transport, in destination decoding, VPP, DMA-BUF import, or final
composition.

The same directory contains sampled stage snapshots under `stages/`. Through
sequence 300, every thirtieth encoded frame writes matching
`stream-N-generation-N-sequence-N-source.ppm` and `-normalized.ppm` files.
`source` is the client DMA-BUF after VA import; `normalized` is the padded NV12
encoder input converted back to XRGB and cropped to visible geometry. Clean
matching snapshots followed by a corrupt software-decoded H.264 frame isolate
the fault to encoding. Stage capture performs extra synchronous VPP work and
file writes in the encoder worker, so timing and credit traces from dump runs
must not be used as ordinary performance measurements.

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
  builds. Compatibility negotiation is deferred until the protocol stabilizes.
- Encoded mode is opaque H.264 only. It has no AV1, VP9, alpha plane, software
  codec fallback, cross-device capability negotiation, or decoded-output pool.
- Decoder generation retirement is queued when transported layers rotate or
  disappear and is applied by the worker before its next decode command; an
  idle worker can therefore retain the last retired VA session until new work
  arrives.
- There is no network transport, clipboard/DnD transfer, or filesystem path
  mediation in this binding.
