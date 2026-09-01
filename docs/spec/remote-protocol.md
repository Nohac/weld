# Remote presentation protocol

## Boundary and status — Direction

This document describes the transport-neutral session and media contract used
to realize [remote window hoisting](remote-hoisting.md). It owns handshake,
capability exchange, transport bindings, surface representation, media-stream
topology, codec selection, adaptation, and failure semantics. Hoist scope,
window-family admission, placeholders, reclaim, and source-authoritative
lifecycle remain defined by the hoisting specification.

No network protocol, encoder, or decoder is implemented yet. Before 1.0, peers
may require an exact protocol revision rather than maintaining compatibility
with earlier revisions. Capability negotiation remains necessary even when
revisions match because devices, transports, targets, and hardware differ.

Wire records use project-owned identities and values. They do not expose Bevy
entities, Smithay resources, Rust ABI details, native handles, wgpu objects,
Iroh streams, WebRTC objects, or codec-library types.

## Orthogonal protocol layers — Direction

The following decisions are independent:

- A **transport binding** establishes connectivity and maps Weld's logical
  flows onto Unix sockets, TLS/TCP, Iroh/QUIC, WebRTC, or another secure
  carrier.
- A **surface mode** carries committed pixels as compatible native buffers or
  as encoded media.
- A **video profile** selects codec, profile, color, alpha, extent, cadence,
  acceleration, and packetization constraints for an encoded stream.
- A **presentation target** describes one destination viewport and its layout,
  scale, refresh, color, input, and quality preferences. One device may expose
  several targets.
- The **logical surface graph** preserves windows, roles, layers, popups,
  relationships, input targets, and lifecycle.
- A **media stream layout** maps one or more regions from that graph into an
  encoded image. It does not redefine the graph.

Native and encoded surface modes can therefore use the same Unix transport,
while encoded media can use Iroh or WebRTC without changing hoist policy. The
existing generic `weld-client` wire projection is compatible with this split,
but its in-process types and current Postcard records are not the remote wire
protocol.

Illustrative configuration might use `--transport`, `--surface-mode`, and a
debug-only `--codec` preference. Those names are not a committed CLI or config
surface; configuration ownership remains in
[Plugins and configuration](plugins-and-configuration.md).

A transport binding owns connectivity, logical-flow mapping, framing,
binding-native feedback, and peer-loss reporting. A media adapter owns native
frame import, GPU color conversion, encode and decode, hardware session limits,
and encoded-payload production. This responsibility split is defined here;
neither adapter exposes its library or platform types through the semantic
protocol.

## Session handshake — Direction

A peer session progresses through explicit states:

```text
transport ready
  -> hello exchanged
  -> device proved and authorized
  -> capabilities exchanged
  -> hoist offered and answered
  -> active
  -> renegotiating or draining
  -> closed
```

`Hello` reveals only the exact protocol revision, endpoint role, connection
nonce, device-identity reference, and authentication mechanisms needed to
continue. Detailed hardware, resources, application catalogs, and mesh state
are not disclosed to an unknown peer.

Transport encryption and Weld authorization are separate. A binding proves
which transport endpoint established the connection. A Weld device then binds
its proof to the connection and both nonces so credentials cannot be replayed
onto another session. The
[identity and mesh
model](identity-and-meshes.md#trust-and-authorization--direction)
determines which members, devices, resources, and actions that endpoint may
represent. The protocol does not duplicate grants or mesh governance.

An unknown but pairable device may receive a bounded `PairingRequired`
outcome. Pairing completes through the wallet or another authorized authority
before protected capabilities or resources are disclosed. Authentication or
authorization failure closes the session without falling back to anonymous
access.

## Capability exchange — Direction

Capabilities are directional and may contain several implementations of the
same codec. A device may hardware-decode AV1, software-decode another AV1
profile, and be unable to encode either. Capability identity must remain
stable for the peer session so a selected path names the exact encoder,
decoder, native importer, and presenter it uses.

Transport capabilities cover at least:

- reliable ordered control and input;
- independent media delivery and feedback;
- optional coalescible or unreliable observations;
- native descriptor passing and supported descriptor kinds;
- maximum record or message constraints; and
- binding-native congestion, path, and peer-loss observations.

Native surface capabilities cover handle kind, DRM format and modifier,
planes, synchronization mechanism, device compatibility, maximum extent, and
whether pixels already reside in CPU memory. Native-buffer selection is valid
only when the binding can transfer the handle and the destination can import
it safely.

Encoded-video capabilities cover at least:

- encode or decode direction;
- codec, profile, level, tier, and packetization;
- hardware or software implementation;
- bit depth, chroma sampling, color range, primaries, transfer, and matrix;
- opaque and alpha modes;
- low-latency and keyframe behavior;
- native input or output formats and GPU import stages;
- dimension alignment and coded-versus-visible crop support;
- concurrent session budget; and
- a set of extent-and-cadence performance points.

A performance point represents a supported combination such as 4K at 30 Hz or
1080p at 120 Hz. A single maximum rectangle is insufficient because codec
levels and hardware are also constrained by pixels or coding blocks per
second. Android's [video capabilities][android-video-capabilities] and
performance-point model are useful prior art, but the Weld record remains
platform-neutral.

Capabilities must distinguish claims derived from a platform API from results
measured by Weld. Software decoding in particular needs a measured sustainable
envelope containing decode latency, missed deadlines, and the tested extent
and cadence. Current thermal, battery, bandwidth, and session occupancy are
dynamic observations rather than permanent capabilities.

## Complete-path selection — Direction

Selection evaluates a complete path:

```text
source buffer import
  -> conversion or composition
  -> encode
  -> transport binding
  -> decode
  -> destination GPU presentation
```

A codec is not eligible merely because both peers name it. Every required
stage must satisfy the presentation target, alpha and color requirements,
extent and cadence, latency policy, native import formats, and current session
budgets. A software fallback is selected explicitly rather than silently
replacing a promised hardware path.

Default policy prefers a complete hardware path. Among similarly accelerated
and otherwise viable paths, it prefers AV1, then VP9, then H.264. Hardware
H.264 encode and decode normally outrank hardware AV1 encode followed by
software AV1 decode for an interactive phone session. A destination may prefer
the mixed AV1 path when measured decode headroom and bandwidth savings justify
its battery, thermal, and latency cost.

The destination owns its power and thermal policy; the source owns encoder
availability; the binding contributes network observations. Candidate user
policies include balanced, data saver, battery saver, quality, and hardware
only. The selected path records both capability identities so telemetry and
renegotiation describe what actually ran. Destination observations follow the
session-scoped privacy and disclosure rules of the
[presentation
target](remote-presentation.md#presentation-target-model--direction).
Dynamic capacity admission and reservation follow
[Remote media budgeting](remote-budgeting.md).

HEVC is deliberately outside the current AV1, VP9, then H.264 product policy.
This is not a claim that the generic capability model cannot carry it. Weld has
not selected an HEVC licensing and distribution policy; it can be added later
as another codec capability without changing the protocol layers.

## Hoist offer and answer — Direction

Capabilities say what a peer could do. A hoist offer identifies the authorized
scope and admission mode, presentation target, selected window and surface
identities, logical geometry, visibility, required alpha and color behavior,
input and data permissions, preferred cadence, and latency policy. It does not
grant anything beyond the already authorized hoist request.

The answer accepts or refuses each required presentation and chooses its
surface mode. Native mode selects a compatible handle and synchronization
contract. Encoded mode selects a complete video profile and initial media
stream layout. Required capabilities fail closed; optional degradation is
reported explicitly to the source admission policy.

A connection exchanges device capabilities once and updates them when hardware
or platform state changes. Profiles are selected per presentation stream, not
globally per device, because a phone screen and attached monitor may require
different extents, cadence, color, and layout.

## Transport bindings and logical flows — Direction

The semantic protocol exposes logical operations rather than one generic byte
stream. Every binding preserves these flows even when it maps them differently:

- **Control and state** carries capabilities, offers, surface graphs,
  lifecycle, configuration, errors, admission, reclaim, and stream generations
  reliably and in order.
- **Input** initially carries pointer, button, key, modifier, focus, gesture,
  and input-state transitions reliably and in order.
- **Media** carries independently droppable access units, timestamps,
  keyframes, damage and region metadata, and optional cursor metadata.
- **Feedback** carries decode and presentation status, latency, queue depth,
  loss, congestion observations, and keyframe requests.
- **Bulk data** carries separately authorized clipboard, drag-and-drop, file,
  and similar byte streams.

Media congestion must not block input or lifecycle control. Explicitly
coalescible absolute pointer observations may later use an unreliable path.
Keys, buttons, modifiers, relative locked-pointer deltas, focus, configure,
and lifecycle transitions remain reliable and ordered.

The current local binding uses Unix sequenced packets and `SCM_RIGHTS` for
native buffers. An encoded local mode may use a separately framed Unix stream
for media so large keyframes do not inherit control-record size limits.
Postcard remains the initial control-record encoding for the local and first
Iroh tracers. It is an encoding choice rather than the semantic protocol or
transport framing, and another binding may implement the same records without
exposing Postcard to hoist policy.

A TLS/TCP binding can provide a simple interoperability or diagnostic path but
must preserve independent logical flow behavior rather than let a saturated
media byte stream block control and input.

[Iroh 1.x](https://docs.iroh.computer/protocols/using-quic) is the intended
first native network candidate because it supplies authenticated endpoint
identity, encrypted QUIC connectivity, direct paths, relay fallback, streams,
datagrams, path changes, and connection observations. Actual discovery,
pairing, mobile lifecycle, relay behavior, and recovery require measurement.
An initial binding uses one connection because independent streams avoid
stream-level head-of-line blocking. Prioritization or several connections remain
available only if saturated media measurably harms control or input latency.

A future [WebRTC](https://www.w3.org/TR/webrtc/) binding may map encoded video
and feedback to RTP/SRTP and RTCP while carrying Weld control and input through
[data channels](https://www.rfc-editor.org/rfc/rfc8831). Its native codec
negotiation and congestion behavior become binding constraints; WebRTC objects
and SDP do not become the Weld semantic protocol.

Every binding preserves exact protocol revision, capability negotiation,
endpoint authorization, and logical-flow semantics. A relay that forwards
end-to-end encrypted traffic remains outside the content trust boundary, though
it observes connection metadata and traffic shape and can deny availability. A
gateway that terminates encryption enters the content trust boundary and must
be explicitly disclosed and authorized.

## Buffer, frame, and queue lifetimes — Direction

Raw-frame and encoded-payload lifetimes are separate. A source
`ClientBufferLease` remains live until every encoder or GPU conversion has
finished reading it and the relevant completion has retired. It does not remain
live until network delivery or destination presentation. The resulting encoded
payload has independent ownership through transport, decode, and presentation.

A control record references a media-frame identity rather than embedding an
arbitrarily large payload. A media-frame identity belongs to one stream
generation and timestamp. Late payloads for retired generations are discarded.
Destination acknowledgement informs latency, dropping, and keyframe policy but
never controls release of the source Wayland buffer.

All crossings into network and codec runtimes are bounded and wake the host
through owned mechanisms. Neither calloop nor Bevy waits for connection,
congestion, encoding, decoding, or remote acknowledgement. Media queues retain
the newest useful work, preserve decoder recovery data, and discard superseded
frames under pressure. Ordered control and input are not discarded by that
policy.

Avoiding raw full-frame CPU readback is a requirement for hardware media paths.
Literal zero GPU copies is an opportunistic fast path. SHM clients are the
explicit source-memory exception because their submitted pixels already reside
in CPU memory.

For simple opaque single-surface content, the source first attempts to import
and retain the exact client DMA-BUF when format, modifier, crop,
synchronization, and encoder support permit it. Cropping, multi-surface
composition, alpha, effects, incompatible formats, or synchronization may
instead require a GPU conversion or composition into an encoder-importable
DMA-BUF. Both paths retain the source lease until GPU or encoder consumption
completes; neither introduces raw-frame CPU readback.

## Surface graph and media stream topology — Direction

The logical graph remains authoritative even when media is packed differently.
A stream layout maps stable presentation or surface identities and source
rectangles into coded regions with destination rectangles, transforms, and
stacking. Input remains addressed to logical surface identities rather than
codec tiles or atlas coordinates.

The first encoded implementation uses one presentation and one region per
stream. Future negotiated layouts may use:

- one stream per surface layer;
- one source-composited stream for a toplevel and its owned surface tree;
- separately positioned popup or transient streams;
- a stable atlas containing several presentations; or
- tiled streams for one presentation beyond a codec performance envelope.

Rapidly alternating unrelated surfaces through one stateful encoder is not a
baseline strategy. Reference history, keyframes, resolution changes, cadence,
and failure state belong to a stream. Prefer persistent encoders for active
presentations, pausing or closing background streams, and a stable atlas only
after measured session limits justify its complexity.

The [budgeting authority](remote-budgeting.md) supplies demand and resource
evidence when recommending a topology change. It does not own stream-region or
atlas mappings.

The mapping can change without changing the surface graph. A new layout uses a
new stream generation and becomes active only when the destination can decode
and map it atomically.

## Adaptive and alpha media — Direction

Encoding policy can use compositor knowledge unavailable to screen capture.
The [budgeting policy](remote-budgeting.md) owns specific focus, recency,
workspace, visibility, occlusion, resource, and fairness decisions.

Focus, visibility, damage, and resource scheduling use actual observations
rather than fixed focus-only rules. A Wayland client is not forced to render at
monitor refresh. Weld encodes the newest valid content when a presentation
opportunity needs it.

Opaque and alpha-capable profiles are negotiated separately. A codec name does
not guarantee transparency. An alpha-capable profile may carry synchronized
color and alpha payloads under Weld framing with shared frame identity,
timestamps, keyframe coordination, missing-alpha behavior, premultiplication,
color-space rules, and recovery. The
[WebM alpha design](https://wiki.webmproject.org/alpha-channel) is useful prior
art without selecting WebM as Weld's container.

Changing transparent content may consume two encoder and two decoder sessions.
Admission accounts for those budgets. Static or sparse alpha updates remain a
future optimization. The hoisting policy owns refusal and user approval for an
opaque degradation when transparency is required.

## Runtime adaptation and renegotiation — Direction

Bitrate, quantization, and resolution should adapt within the selected codec
before replacing it. Sustained bandwidth, decoder, thermal, session-budget,
target, alpha, or hardware failure may require a new path.

A path or layout switch:

1. negotiates a new profile and stream generation;
2. begins with codec configuration and a keyframe;
3. keeps the old generation valid until the replacement is decodable;
4. activates the replacement atomically; and
5. retires the old generation and ignores its late payloads.

Hysteresis and a minimum dwell prevent codec or resolution flapping. Peer loss,
decoder failure, or exhausted recovery state is reported to source hoist policy
so it can reconnect, reclaim, or fail visibly rather than freezing stale media.

## Codec and media implementation candidates — Exploration

[FFmpeg](https://ffmpeg.org/ffmpeg.html) with VA-API is the selected initial
Linux implementation for H.264 and AV1. cros-codecs remains useful as an
independent diagnostic reference, and narrower native or Vulkan Video backends
may implement the same contract later.
Android destinations should evaluate MediaCodec behind the same neutral media
contract rather than expose an Android API through the wire protocol.

The experimental [iroh-live](https://github.com/n0-computer/iroh-live)
workspace is serious reference material because it combines Iroh, MoQ, Linux
VA-API, Android MediaCodec, hardware-buffer presentation, and Dioxus adapters.
Its components and current capabilities should be evaluated or ported
independently against Weld's contracts rather than adopted as one stack.

Any backend must accept Weld-owned native frames, report truthful stage and
session capabilities, preserve per-stream lifetime, and never move a promised
hardware path onto the compositor thread or an unreported CPU fallback.

## GPU-resident network handoff — Exploration

The longest useful fast path keeps large frame data outside CPU memory:

```text
client DMA-BUF -> hardware codec -> device-resident encoded payload -> NIC
```

The first edge can import a compatible client buffer or consume a
GPU-composited encoder buffer. The final edge is a separate capability: an
encoder and network adapter must agree on device-visible output memory,
synchronization, packetization, and ownership. Linux DMA-BUF supplies
cross-device sharing and fences but is not a network transport.

[NVIDIA DOCA GPUNetIO](https://docs.nvidia.com/doca/sdk/doca-gpunetio/) proves
that compatible NVIDIA GPUs and ConnectX or BlueField NICs can transmit from
GPU-visible memory. It is a hardware-specific candidate, not Weld's baseline.
Equivalent Intel, AMD, other-NIC, and mobile paths remain open research.

Negotiate the stages independently:

- source import or composition into encoder-compatible memory;
- hardware encoding without raw-frame CPU readback;
- encoded payload retained in device- or NIC-visible memory; and
- framing, encryption, and transmission without ordinary CPU staging.

An initial secure binding may move the much smaller compressed bitstream
through CPU-visible memory. That remains acceptable because it avoids raw-frame
readback. A GPU-network path must preserve identical authentication,
authorization, encryption, and lifetime semantics.

## First encoded local tracer — Exploration

The first tracer extends the existing Unix sibling-process scenario with two
surface modes: native descriptors and encoded media. Both use the same hoist,
surface, input, configure, reclaim, and failure semantics.

Encoded mode begins with one active opaque presentation, one stream region,
a usable client-declared crop under the
[window-geometry
policy](remote-presentation.md#window-geometry-and-visual-overflow--direction),
bounded newest-frame queues, and a hardware-only raw-frame path. H.264 may be
the first implemented fallback for the tracer because its cross-platform
hardware path is easiest to validate. AV1 and VP9 remain ordered product
preferences but are not advertised until their real encode, decode, and
presentation paths work.

The tracer applies the
[initial budgeting
matrix](remote-budgeting.md#initial-measurement-matrix--exploration)
to context count, extent, cadence, conversion surfaces, and resize generations.

The tracer verifies native-buffer completion after encoding, decoded-buffer
completion after destination GPU use, codec configuration and keyframe
recovery, exact control ordering, input and popup behavior, destination-driven
configure and scale, reclaim, peer loss, and absence of raw-frame CPU readback.
Native mode remains the same-machine correctness and latency baseline.

## Staged network and mobile validation — Exploration

After the local tracer:

1. Measure a retained Iroh link between a laptop on Wi-Fi and an Android phone
   on 5G. A one-time development ticket authorizes exactly the authenticated
   endpoint for that run without becoming the production pairing model. Record
   direct versus relayed path, establishment time, round-trip time, throughput,
   path changes, reconnect, and input-sized messages.
2. Carry one opaque toplevel between Weld processes over Iroh using the same
   semantic protocol, independent control/input/media flows, and selected
   hardware profile.
3. Replace the Weld destination with a minimal Android destination. Decode into
   an Android hardware buffer and compare a custom WGPU paint source with a
   native EGL surface. Reject per-frame raw CPU readback.
4. Validate destination-driven size, scale, orientation, input, reclaim, loss
   recovery, and thermal or software-decoder renegotiation while the phone
   remains on 5G.

Every phase records path, codec capabilities and selection, source and
destination acceleration, encoder and decoder latency, queue depth, dropped
frames, keyframes, CPU and GPU copies, cadence, throughput, and input latency
during saturated media. Neither calloop nor Bevy may block on codec or network
work.

## Open work — Exploration

- Define binding-specific framing around the initial Postcard records and
  select the exact-revision identifier.
- Define binding-independent device proof over transport session transcripts.
- Use validation evidence to select production Iroh discovery, relay, pairing,
  flow mapping, and reconnect policy.
- Decide media packetization and feedback mappings for Iroh and WebRTC.
- Validate hardware AV1, VP9, and H.264 through the same native-frame contract.
- Extend GPU capture and encoder interop beyond the initial opaque tracer to
  crop, composition, effects, and alpha without raw-frame CPU readback.
- Define stream handoff between destination targets and between peer devices.
- Use [budgeting evidence](remote-budgeting.md) to decide when
  per-presentation streams, pausing, atlases, or tiling justify a topology
  change.

[android-video-capabilities]: https://developer.android.com/reference/android/media/MediaCodecInfo.VideoCapabilities
