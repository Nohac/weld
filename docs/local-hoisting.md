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
reference policy remains the next diagnostic target; the tracer should compare
high-bitrate and all-keyframe modes before attributing the damage to a codec or
switching to AV1.

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
```

`--trace-media` keeps ordinary dependencies at `info` while enabling structured
source-batch, destination-decode, coalescing, and credit timing for the encoded
boundary. For example, 60 surface-tree commits per second with two changed
layers requests 120 sequential codec frames per second in the current tracer;
the trace records both the commit and layer counts so that multiplication is
visible separately from per-frame decode time.

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
- There is no network transport, clipboard/DnD transfer, or filesystem path
  mediation in this binding.
