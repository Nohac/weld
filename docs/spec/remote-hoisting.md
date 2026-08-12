# Remote window hoisting

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

No remote transport, encoder, decoder, or hoisting state is implemented yet.

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

Each independently transported window retains its own placeholder and reclaim
state. UI may visually aggregate those placeholders for a workspace or desktop
session only if the underlying per-window identities, layout positions, and
reclaim actions remain recoverable.

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
- Hardware support and session budgets for AV1, VP9, H.264, and paired alpha
  payloads on representative Intel, AMD, NVIDIA, and mobile devices.
- Per-application PipeWire audio and clipboard semantics.
- Congestion and fairness policy for several simultaneous windows.
- Workspace identity, filtering, mirroring, and local/remote layout meld rules.
- Destination handoff and whether one hoist may move between peers without a
  source-local round trip.
