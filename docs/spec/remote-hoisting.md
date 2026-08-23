# Remote window hoisting

## Local loopback lifecycle — Implemented

The optional `weld-hoist` plugin implements the first source-side lifecycle
proof. `Super+H` replaces the focused occupied managed window's ordinary local
presentation with a hoist-owned placeholder and Reclaim control while keeping
the real client occupant alive. A second managed window becomes the effective
client-policy endpoint while borrowing that occupant: it uses ordinary CSD or
SSD, presents the same GPU-imported surface and its popups, routes input and
client move/resize interactions, and supplies its own output membership,
preferred scale, and configure size. Moving or resizing the source placeholder
does not affect the client. Reclaim or receiver loss restores policy and
presentation on the same source frame without changing its occupant or
geometry. Explicit reclaim first hides and retargets the receiver to the
placeholder's output and inner size, then waits for the resulting client
configure to settle before revealing the source again. A bounded fallback
prevents an unresponsive client from trapping the source in that transition.

This is a same-process loopback and deliberately has no serialization,
networking, codec, peer identity, or authorization. It directly samples the
source image rather than publishing a transport frame. Related independent
xdg-toplevels are grouped by their client-declared parent chain. Existing and
later mapped descendants join an active local family as independent
source/receiver pairs; popups and subsurfaces continue following their owning
toplevel's surface tree. Reclaim from any member freezes admission, stages the
captured members, and restores the family together. A member that leaves the
declared parent chain is staged back independently. Unmapping a source removes
that member, while loss of the family root restores the remaining family.

Family members already present when hoisting begins each retain a source
placeholder because each occupied host layout. Related toplevels first created
afterward are receiver-only and create no new source placeholder. If a captured
client toplevel is destroyed remotely, its retained slot becomes a "Window
closed remotely" tombstone with Dismiss but no Reclaim action. Reclaiming other
live members leaves that tombstone intact. A protocol unmap is not treated as
destruction: it ends that member's hoist session and allows an eventual remap
to use ordinary local presentation.

This implemented relation is deliberately narrower than application or
process inference. Unparented windows from the same executable or app ID do
not automatically join. A newly mapped related dialog can be presented locally
for one frame before window admission and follow-family policy observe it; that
prototype transition remains to be tightened. No remote transport, complete
transient policy, or non-xdg family inference is implemented.

## Hoisting layers and crate boundaries — Direction

The local proof now separates its neutral relay, Weld application integration,
and placeholder scene. The present crates establish dependency direction, but
they do not yet define the remote wire or media protocol:

- `weld-hoist-core` owns the runtime-independent hoist identities and current
  same-process loopback adapter. It must remain free of Bevy, Smithay, wgpu,
  codec, and network dependencies as the stable session, preference, lifecycle,
  control, input, media, and transport contracts are developed.
- `weld-hoist` is the Weld application integration. It projects the core
  domain into `weld-app` and managed-window components, translates between
  stable client identities and live entities, and owns window-family admission
  and reclaim orchestration. It is the optional Bevy plugin, not the owner of
  the wire protocol, client buffers, or placeholder visuals.
- `weld-hoist-ui` is the optional BSN presentation plugin. It supplies source
  placeholders, reclaim and dismiss controls, connection status, and other
  user-facing scenes by attaching children to managed windows through the
  public hoist state and actions. SSD must not own or special-case those
  controls.

Transport and codec implementations remain replaceable adapters around
`weld-hoist-core`; concrete crates should be introduced only when their
dependencies and runtime boundaries are known. A headless source can therefore
combine `weld-core`, `weld-hoist-core`, and selected transport and media
adapters without constructing Bevy or `weld-hoist-ui`. A browser, mobile, or
native destination can implement the stable protocol without linking any Weld
crate.

## Scope — Direction

Hoisting relocates the interactive presentation of windows, not their
processes. The source Weld instance retains the real Wayland clients and
remains authoritative for protocol state. A compatible destination endpoint
presents each remote window. A Weld or platform-native compositor integration
may manage those presentations using the same placement, focus, decoration,
clipping, and animation policy as local windows; simpler destinations may
present them inside their own application UI.

The independently managed transport unit is a mapped toplevel and its owned
surface tree. Protocol-owned popups and subsurfaces remain attached to that
unit rather than becoming ordinary independent windows. Related transient
toplevels can be transported independently while retaining their relationship
to a root toplevel. Together, the root and those related presentations form a
**window family**. This follows the role distinctions in
[Surfaces and input](surfaces-and-input.md#window-management--direction) and
the [managed-frame model](window-management.md#managed-frame-model--direction).

A hoist session can select one of three scopes:

- **Single window family** — one selected toplevel, its surface tree and
  popups, and any related toplevels admitted by policy.
- **Virtual workspace** — all admitted windows in an i3- or Sway-like logical
  workspace.
- **Desktop or session** — all admitted windows across the source desktop.

Workspace and desktop scopes still transport separate window presentations.
They do not flatten the source into one screen-capture stream. The protocol
must preserve stable window and family IDs, ownership and transient
relationships, workspace membership, stacking, visibility, geometry,
configure state, and lifecycle transitions. A destination may either mirror
the remote workspace structure or meld remote windows into local workspaces;
that placement policy is not yet selected.

No remote transport, encoder, or decoder is implemented yet.

## Endpoint policy projection — Direction

A transport does not copy native monitor objects or make destination state
authoritative at the source. It carries stable-ID preferences and observations
that the source validates and projects into its own window policy. A
destination presentation may contribute logical content size, scale and output
characteristics, refresh and color capabilities, decoration support, focus,
and visibility. The source remains authoritative for client lifetime,
configure sequencing, buffer ownership, admission, reclaim, and the accepted
result of those preferences.

The transported presentation is a role-preserving surface family rather than
an undifferentiated image. A toplevel owns its subsurface tree and popups;
related transient toplevels retain separate identities and relationships.
Menus and tooltips therefore follow the destination presentation without
becoming freely managed windows. Input is addressed to those stable surface
identities and transformed from destination-local coordinates at the source.
Media revisions, damage, synchronization, and popup/tree state use the same
identity graph but remain logically separate from ordered control and input
flows.

The local loopback is expressed as an ordinary `weld-client` adapter with a
Relocated source namespace and runtime route aliases back to the authoritative
source. That adapter is only an in-process validation of the endpoint model;
its commands and erased buffer access are not a wire API. Bevy entities,
Smithay objects, native graphics handles, and wgpu internals stay behind
endpoint adapters.

## Endpoint roles and portability — Direction

The source endpoint acts as the server and must be Weld because it owns the
Wayland clients, compositor state, capture path, configure translation, input
routing, and reclaim authority. The receiving client is the **destination
endpoint**. It does not need Smithay, Bevy, wgpu, or a Weld compositor and may
be another Weld instance, a native desktop application, a browser, a mobile
application, or another compatible implementation.

Destination diversity is expressed through the same versioned capability
negotiation used for codecs, alpha, input, and other optional features. A
destination that cannot satisfy a required capability produces a refused or
explicitly degraded session under the existing admission rules; it does not
receive a silent exemption. Full compositor integrations may expose remote
presentations as independent local windows and meld them into local
workspaces. Simpler clients may arrange those presentations within one viewer
while preserving their protocol identities and relationships.

Wire contracts must use project-owned serialized IDs and messages. They must
not expose Smithay or Bevy types, Rust ABI details, native graphics handles,
or wgpu internals. Destination form does not change source authority,
per-window identity, placeholder behavior, or reclaim guarantees.

## Admission and follow policy — Direction

Scope says what a session may include. Admission mode separately says whether
later windows join it:

- **Snapshot** admits only the matching independent toplevels present when the
  hoist begins.
- **Follow family** also admits later related toplevels belonging to the
  selected window family.
- **Follow scope** admits later windows that enter the selected workspace or
  desktop scope and pass its filters.

Admission also depends on satisfying the required media profile. In
particular, content that can contribute transparency is subject to the
alpha-capable media requirements below.

Follow-scope access is an explicit, broad disclosure and auto-hoist grant. Its
filters and current membership must be visible and revocable; windows excluded
by policy must not be disclosed to the peer. A revoked admission restores the
window at its authoritative source.

The owned surface tree, including later popups and subsurfaces, always follows
an admitted toplevel so its interaction remains coherent. Those roles do not
become freely placeable windows. A newly created independent toplevel is a
separate presentation even when follow-family policy admits it.

## Transport binding and Iroh — Direction

The hoisting protocol is independent of a particular transport API. Weld
intends to evaluate [Iroh](https://docs.iroh.computer/) as the primary native
peer-to-peer binding. Its cryptographic node identity and QUIC connectivity are
attractive for encrypted connections without requiring users to manually
configure static addresses, ports, domains, or a separate VPN. Actual
discovery, relay, pairing, trust, and offline behavior must be validated
against Iroh before the choice becomes an implemented dependency.

Browsers or constrained platforms may require another secure transport binding
or a compatible gateway. Every binding must preserve protocol versioning,
capability negotiation, endpoint authorization, and independent flow
semantics. A gateway that only relays end-to-end encrypted traffic remains
outside the content trust boundary. A gateway that terminates encryption gains
access to media, input, and control data, is inside that boundary, and must be
explicitly disclosed and authorized. Pairing and hoist authority remain
anchored to destination identity rather than being implicitly delegated to a
gateway.

One connection should multiplex logically independent flows:

- **Control and state** — pairing, authorization, capabilities, window
  metadata, lifecycle, configure requests, errors, and reclaim transitions.
- **Media** — encoded frames, timestamps, keyframe and damage metadata, and
  optional cursor metadata.
- **Input** — low-latency pointer motion plus ordered button, key, modifier,
  focus, and input-state transitions.

Logical separation is a protocol obligation even when a transport maps flows
onto several QUIC streams, datagrams, or other primitives. Media congestion
must not block input or lifecycle control.

## Hoist and reclaim lifecycle — Direction

When a destination accepts a hoist, the source keeps each original window's
identity, workspace membership, layout position, and recoverable local state.
Its live client texture is no longer composed into the local desktop and is
replaced by a compositor-owned placeholder with application metadata,
connection state, and a **Reclaim** action.

In the managed-frame model, the source frame retains its real client occupant
while its local presentation changes to the remote/reclaim state. It is not a
vacant frame, and reclaim restores local presentation on the same frame.

Each window that occupied source layout when transport began retains its own
placeholder and reclaim state. Later follow-family or follow-scope admissions
need not manufacture source slots they never occupied. UI may visually
aggregate preserved placeholders for a workspace or desktop session only if
the underlying per-window identities, layout positions, and reclaim actions
remain recoverable.

The placeholders continue participating in layout so hoisting does not
collapse the source workspace. Reclaim, destination departure, authorization
revocation, or unrecoverable connection loss restores local presentation. A
short reconnect policy may preserve the remote placement, but source recovery
must not depend on the destination remaining available.

The destination may request a new logical content size. The source translates
that request into a client configure and streams the eventual committed size;
interactive destination resize may temporarily scale the most recent frame.

## Adaptive media — Direction

Encoding policy can use compositor knowledge unavailable to ordinary screen
capture:

- focused and actively changing windows favor high cadence and low latency;
- visible background windows may reduce cadence or increase compression;
- fully obscured windows may pause frame encoding while retaining state; and
- measured QUIC throughput, loss, and queueing may adjust bitrate, resolution,
  cadence, or keyframe policy.

These are policies over actual visibility and damage, not fixed focus-only
rules. A client is not forced to render at the monitor refresh rate; Weld
encodes the newest valid content when a presentation opportunity needs it.

AV1 is the preferred initial hardware encode/decode target and VP9 is the next
target, with H.264 as the compatibility fallback. Selection must still use the
actual low-latency capabilities and session limits of both peers. Opaque and
alpha-capable profiles are negotiated separately, so a machine may prefer a
different codec for each profile.

A codec name alone does not guarantee transport of transparent pixels. For an
alpha-capable profile, Weld should carry color and alpha as independently
encoded payloads under its own QUIC media framing. The source determines
whether presented content can contribute transparency from its imported buffer
format and compositor-known opaque coverage. Such content requires an
alpha-capable profile. If the peers cannot provide one, admission fails by
default; degrading to opaque presentation must be visibly disclosed and
explicitly approved by the user at the authoritative source. The
[WebM alpha-channel design](https://wiki.webmproject.org/alpha-channel) is
useful prior art for this split, but does not imply that Weld adopts WebM as
its transport container. The framing must define shared frame identity and
timestamps, keyframe coordination, missing or dropped alpha behavior,
premultiplication and color-space rules, and loss recovery.

Changing transparent content may therefore consume two concurrent hardware
encode sessions at the source and two decode sessions at the destination.
Admission and adaptation policy must account for those limits across every
hoisted window. Reusing a static alpha plane or sending sparse alpha updates is
an optimization to validate later, not a baseline guarantee.

[cros-codecs](https://docs.rs/cros-codecs/latest/cros_codecs/) is a possible
hardware codec interop layer; [FFmpeg](https://ffmpeg.org/ffmpeg.html) with
Linux VA-API is another candidate. Neither is a selected dependency. The
eventual abstraction must expose codec, profile, pixel-format, modifier,
alpha, and concurrent-session capabilities, and permit a software fallback
without silently moving a supposedly hardware path onto the compositor thread.

For simple opaque single-surface content, an encoder should first attempt to
import and retain the exact client DMA-BUF when its format, modifier, and
synchronization are supported. A family requiring composition, alpha, effects,
or RGB-to-encoder-format conversion should render into an encoder-importable
DMA-BUF on the GPU. Avoiding a full-frame CPU pixel copy is a requirement;
literal zero GPU copies is an opportunistic fast path. SHM clients are the
explicit source-memory exception because their submitted pixels already reside
in CPU memory.

### GPU-resident network handoff — Exploration

The longest useful fast path keeps large frame data outside CPU memory:

```text
client DMA-BUF -> hardware codec -> device-resident encoded payload -> NIC
```

The first edge can directly import a compatible client buffer or consume a
GPU-composited encoder buffer. The final edge is a separate capability: an
encoder and network adapter must agree on device-visible output memory,
synchronization, packetization, and ownership. Linux
[DMA-BUF](https://docs.kernel.org/driver-api/dma-buf.html) supplies cross-device
buffer sharing and fence primitives, but it is not itself a network transport.

[NVIDIA DOCA GPUNetIO](https://docs.nvidia.com/doca/sdk/doca-gpunetio/) is
evidence that a GPU can control compatible NIC queues and transmit from GPU
memory without a CPU staging copy. Its preferred GPU-memory mapping uses
DMA-BUF, but the documented path requires a supported NVIDIA GPU software
stack and a ConnectX or BlueField NIC. It is therefore a hardware-specific
candidate rather than Weld's baseline network abstraction. Equivalent paths
for Intel, AMD, other NICs, and mobile devices remain to be researched.

This optimization must be capability-negotiated in independent stages:

- importing or composing the source into encoder-compatible device memory;
- hardware encoding without raw-frame CPU readback;
- retaining the encoded payload in device- or NIC-visible memory; and
- framing, encrypting, and transmitting it without moving that payload through
  ordinary CPU memory.

The media and transport contracts must expose each stage rather than advertise
one ambiguous `zero_copy` flag. An ordinary secure transport such as Iroh may
initially require the much smaller compressed bitstream in CPU-visible memory;
that remains an acceptable fallback because it avoids the expensive raw-frame
readback. A GPU-network fast path must preserve the same authenticated and
encrypted wire protocol and must not bypass authorization merely to avoid a
copy. Exact DMA-BUF lifetime and fence ownership remain mandatory until every
encoder and network consumer has finished with a frame.

## Launcher federation — Direction

An authenticated protocol should let a launcher combine local applications
with application catalogs advertised by trusted Weld peers. A remote launch
request selects a catalog entry and launch environment or profile, then
associates the resulting windows with an explicit hoist scope and admission
mode. Catalog visibility, permission to launch, launch environment, and
permission to hoist are separate capabilities.

The launch response should issue a single-use, time-bounded correlation token
scoped to one hoist session. It may correlate resulting windows with the
launch request, but must not itself grant **Follow scope** access. If one
launch produces multiple independent toplevels, their admission follows the
declared **Snapshot**, **Follow family**, or **Follow scope** policy. Process
ancestry and application IDs are supporting evidence rather than authority;
the gaming-specific handoff constraints are described in
[Gaming sandbox](gaming-sandbox.md#launcher-and-game-handoff).

This protocol is intended to make local and remote apps feel like one launcher
catalog without making remote execution indistinguishable in security UI or
silently exposing applications that a peer was not authorized to discover.

## Security and recovery — Direction

- Pair peers explicitly and persist revocable cryptographic identities.
- Authorize each hoist; treat **Follow scope** admission for a workspace or
  desktop as an explicit broad grant rather than a consequence of launching
  one app.
- Scope forwarded input to admitted windows or an explicit remote seat.
- Negotiate clipboard, audio, file transfer, and gamepad injection as separate
  capabilities.
- Never expose the local Wayland socket, Smithay objects, or unrestricted
  synthetic input to a peer.
- Treat any transport-terminating gateway as an explicitly authorized content
  trust boundary.
- Bound media queues toward recent frames and keep recovery state at the
  authoritative source.
- Trace connection, admission, launch, hoist, and reclaim transitions with
  stable peer and session IDs.

## Open work — Exploration

- Iroh discovery and relay behavior across realistic networks.
- GPU capture and encoder interop without unnecessary full-frame copies.
- Device-resident encoded output and GPU-to-NIC transmission, including
  GPUNetIO-class hardware constraints and compatibility with authenticated,
  encrypted transport framing.
- Hardware support and session budgets for AV1, VP9, H.264, and paired alpha
  payloads on representative Intel, AMD, NVIDIA, and mobile devices.
- Per-application PipeWire audio and clipboard semantics.
- Congestion and fairness policy for several simultaneous windows.
- Workspace identity, filtering, mirroring, and local/remote layout meld rules.
- Destination handoff and whether one hoist may move between peers without a
  source-local round trip.
