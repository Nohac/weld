# Remote presentation protocol

## Boundary and status — Direction

This document describes the transport-neutral session and media contract used
to realize [remote window hoisting](remote-hoisting.md). It owns handshake,
capability exchange, transport bindings, surface representation, media-stream
topology, codec selection, adaptation, and failure semantics. Hoist scope,
window-family admission, placeholders, reclaim, and source-authoritative
lifecycle remain defined by the hoisting specification.

An initial Iroh binding now carries the implemented record subset and opaque
encoded surfaces between two Weld processes. It validates exact revision,
roles, and a source-selected codec over authenticated encrypted QUIC, but is
still a development tracer rather than the complete handshake described here:
the listener checks the authenticated destination against an explicitly
approved EndpointId exchanged through private local files, while the destination
authenticates the source from its trusted ticket. This transport-peer approval
is not the device proof or mesh authorization below. Discovery policy, pairing,
general capability negotiation, reconnect, and dynamic peer admission remain
unimplemented. The
same `weld-hoist-protocol` records are also used by
loopback and the Unix tracer. Before 1.0, peers require an exact protocol
revision rather than maintaining compatibility with earlier revisions.

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
existing generic `weld-client` wire projection and the current
`weld-hoist-protocol` envelopes are compatible with this split. They form the
implemented surface/control record substrate, not the complete remote
handshake, authorization, capability, adaptation, or recovery protocol
described here.

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

Presentation-view capabilities describe supported view counts, packed view
layouts, source rectangles, eye selection, and whether mono can be derived from
a multiview source. They are independent from native versus encoded transport:
the same logical stereo view set may travel as a native DMA-BUF, one packed
encoded frame, or several synchronized streams. Source capability reflects what
the application has actually advertised, not what Weld could synthesize by
duplicating a mono image.

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
identities, logical geometry, visibility, desired or required view
configuration, required alpha and color behavior, input and data permissions,
preferred cadence, and latency policy. It does not grant anything beyond the
already authorized hoist request.

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

Input coordinates are scoped to an authorized logical seat and stable
presentation, not implicitly to a shared source desktop. A negotiated pointer
mode distinguishes target-absolute coordinates, surface-local projection, and
relative deltas captured by the focused surface. Each focus transition carries
an ordered epoch or equivalent generation; motion, buttons, grabs, and releases
name that state so delayed input cannot land on a newly focused window. A
source- or destination-attached device follows the shared
[input-producer contract](surfaces-and-input.md#input-producers-and-remote-control).
The transport projects that seat/focus model; it does not invent another one.

A session-wide handoff does not weaken input recovery. The source retains a
local emergency reclaim action outside the transported shortcut namespace.
Revocation, peer loss, or reclaim first cancels remote focus and held input,
then restores source presentation. Cursor visualization may remain
destination-owned even when the physical mouse is attached to the source;
cursor state and client-selected shape are synchronization data, not authority
to inject input.

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

One logical presentation may contain a synchronized view set. A packed stereo
buffer can map its left- and right-eye rectangles into one coded region and one
access unit, preserving one encoder context and atomic timing while accounting
for the combined pixel rate. Separate per-view streams are negotiated only when
extent limits, independent quality, or a demonstrated hardware path justify
their additional contexts and synchronization. Changing mono or multiview
interpretation rotates the affected stream generation only after the matching
application commit is accepted.

Fixed view sets require only configuration and synchronized frame identity.
Head-tracked view sets add a time-sensitive request/result exchange. The
destination sends a session-relative reference space, predicted display time,
request identity, per-view pose and field of view, recommended extent, and
application transform. The returned frame group acknowledges that request and
records the actual rendered views. A destination can therefore discard stale
results or late-reproject them without treating head pose as pointer input.
Tracked output is normally rendition-specific per independently moving viewer;
compatible fixed stereo can be shared.

A spatial frame group may synchronize color with optional alpha and depth.
Those planes may use distinct payloads, codec profiles, resolutions, or
cadences, but they carry the same frame-group identity and explicit missing-
plane behavior. A packed color view set remains one access unit where practical.
Alpha and depth are never authority for input routing: the logical surface and
its explicit input region remain authoritative.

Foveated streaming is expressed as a media layout over that same presentation,
not as another surface or input target. A base region covers the complete mono
or per-eye view at a lower detail level. One or more enhancement regions carry
higher detail for gaze-local rectangles. Every region records its frame-group,
eye or view, source crop, destination rectangle, quality role, and gaze-request
identity. The destination can always present the base alone; enhancement loss,
lateness, or decode failure is a local quality degradation rather than a stream
failure.

Capability negotiation describes whether an endpoint supports separate base
and enhancement streams, scalable or tiled codec layers, codec-native ROI or
quantization maps, maximum active regions, alignment, per-eye association,
decoder composition, and the additional context and pixel-rate cost. A
destination reports only a normalized, optionally quantized gaze region with
sample and predicted-display timing. Raw gaze history is neither required nor
transported by default. These observations use latest-value delivery and cannot
block ordered control or input.

Each presentation retains its own base-quality state. Focus and attention can
promote one presentation's existing media path and attach or enlarge its
enhancement without rebuilding unrelated presentation streams. Normal gaze
movement updates region coordinates inside the accepted layout; it does not
rotate stream generations or perform full capability negotiation per sample.
Promotion, demotion, and enhancement handoff carry enough ordering information
that a late gaze sample cannot sharpen a previously focused window.

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

## Auxiliary color and alpha atlases — Exploration

An auxiliary media pool may serve both independently positioned popup/tooltip
color and alpha masks for several presentations. It may use several streams
grouped by update cadence and negotiated quality, rather than forcing every
auxiliary image into one stream. It encodes composed stable atlas images, not
unrelated surfaces alternated through a predictive encoder without mappings.

Every tile maps to an authorized logical surface and color/alpha role. Grouping
must respect recipient permissions: a receiver must never obtain an atlas with
another recipient's private pixels. Clear reused tiles and define sampling
boundaries/padding so filtering and stale mappings cannot expose adjacent or
previously assigned content. Popup geometry, input, stacking, clipping, and
lifetime stay independent of the atlas; a popup may extend beyond its parent.

Explore these negotiated alpha representations:

- implicit opaque alpha, with no mask pixels;
- a constant opacity value for genuinely uniform alpha;
- lower-resolution filtered masks for soft shadows and gradients;
- higher-resolution or lossless masks for sharp cutouts, corners, and text;
- retained unchanged masks, referenced by explicit mask revision rather than
  retransmitted with each color update.

Alpha may be carried as grayscale data in a supported video plane, but range
mapping, precision, transfer behavior, filtering, and premultiplication must be
specified independently from color. A mask carries opacity, not arbitrary
shadow color. Known Weld-owned effects could instead be generated at the
receiver; arbitrary client artwork is not replaced by guessed geometry.

Color references the compatible mask revision and accepted layout generation.
Resize, tile reuse, regrouping, loss, and recovery must not apply an old mask to
new geometry or another surface. Static-mask reuse must be explicitly allowed;
missing required alpha follows the negotiated hold/recovery policy, never an
implicit opaque fallback. Layout transitions activate atomically through the
existing generation contract. Different color/mask resolutions retain precise
source-to-destination mappings and do not change input coordinates.

The [budgeting exploration](remote-budgeting.md#codec-pools-and-auxiliary-capacity--exploration)
owns cadence/focus grouping recommendations and resource reservations. The
protocol owns these representations and compatibility rules. This does not
enable alpha, atlases, or new encoders in the current opaque implementation.

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

Regional quality can vary without changing presentation identity. A foveated
base-plus-enhancement layout, codec ROI map, visibility tile, or other regional
scheme is selected by capabilities and budgeting. The protocol specifies the
region mapping and synchronization while the media backend owns how it realizes
the requested quality. A gaze region is scheduling data only: it cannot focus a
window, authorize input, or reveal raw eye coordinates unnecessarily.

## Runtime adaptation and renegotiation — Direction

The [streaming-budget plan](../remote-budgeting-plan.md) defines
the next bounded feedback/allowance and rate-actuation slice. Applied media and
actual presentation must remain distinct observations; reasoned pressure
feedback must not reinterpret surface cancellation as network loss.

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

1. Carry one opaque toplevel between Weld processes over Iroh using the same
   semantic protocol, independent control/input/media flows, and selected
   hardware profile. The initial same-device direct tracer implements this
   step; cross-device validation remains.
2. Measure a retained Iroh link between a laptop on Wi-Fi and a second device
   over a direct or relayed path. A one-time development ticket communicates
   the source address and identity for that run without becoming the production
   pairing model or an enforced admission proof. Record establishment time,
   round-trip time, throughput, path changes, reconnect, and input-sized
   messages.
3. Replace the Weld destination with a minimal Android destination, initially
   on a convenient local network. The current preferred experiment is
   [Godot/Rust on a phone before XR](distributions.md#godotrust-phone-first-xr-client).
   Validate native decoded-buffer presentation without per-frame raw CPU
   readback, then repeat on 5G. Godot external textures, a native surface, or a
   later alternative shell must remain behind the same destination contract.
4. Validate destination-driven size, scale, orientation, input, reclaim, loss
   recovery, and thermal or software-decoder renegotiation while the phone
   remains on 5G.

Every phase records path, codec capabilities and selection, source and
destination acceleration, encoder and decoder latency, queue depth, dropped
frames, keyframes, CPU and GPU copies, cadence, throughput, and input latency
during saturated media. Neither calloop nor Bevy may block on codec or network
work.

## Open work — Exploration

- Evolve the initial bounded Iroh framing and exact-revision handshake with
  authorization, negotiated capabilities, recovery, and observability.
- Define binding-independent device proof over transport session transcripts.
- Define logical-seat, focus-epoch, pointer-mode, and bidirectional cursor-state
  records for the shared [input-producer
  contract](surfaces-and-input.md#input-producers-and-remote-control).
- Include application-requested pointer lock/confinement, relative deltas,
  activation/denial/revocation and safe escape/reclaim/disconnect behavior in
  that input contract. Cursor hiding alone is not capture. Blender's held-LMB
  numeric-value drag is a concrete validation case; see
  [pointer capture](surfaces-and-input.md#pointer-capture-and-relative-motion--direction).
- Define fixed and tracked view-set records, relative reference spaces,
  view-request deadlines, rendered-pose acknowledgement, and synchronized
  color/alpha/depth frame groups.
- Define base and foveal-enhancement region records, latest-value gaze requests,
  base-only recovery, and capability mapping for separate streams versus
  codec-native ROI or scalable layers.
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
