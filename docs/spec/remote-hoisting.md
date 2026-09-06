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
prototype transition remains to be tightened. A same-machine sibling-process
native-buffer transport now exists as an architectural validation binding. It
retains DMA-BUF allocations without a pixel copy and carries already-copied SHM
pixels through sealed descriptors. An initial Iroh binding carries opaque
encoded surfaces between sibling Weld processes. Complete pairing, dynamic
peer admission, transient policy, and non-xdg family inference are not
implemented.

## Hoisting layers and crate boundaries — Direction

The proof now separates its neutral relay, Weld application integration,
placeholder scene, encoded scheduling, and concrete process transports. The
present crates establish dependency direction while leaving the wider remote
protocol open:

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
- `weld-hoist-local` is the current same-machine validation binding. It uses
  Unix sequenced packets and SCM_RIGHTS to relay native buffers between sibling
  Weld processes. DMA-BUF uses a bind-once descriptor path without a pixel
  copy; SHM uses an explicit sealed-descriptor CPU-copy path. It is
  deliberately separate from `weld-hoist-core`.
- `weld-hoist-encoded` owns transport-neutral encoded commit scheduling and
  codec worker contracts. Both Unix and Iroh bindings use these ports.
- `weld-hoist-iroh` owns the initial authenticated QUIC connection, bounded
  control and media framing, and runtime bridge. Private local identity exchange
  approves the intended transport peer; it is not yet Weld device authorization.

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

The initial Iroh transport and FFmpeg VA-API encoder/decoder now provide an
encoded Weld-to-Weld tracer. The local sibling-process binding remains the
native-buffer baseline for adapter, buffer-lifetime, input, configure, scale,
reclaim, and failure-recovery behavior. Neither tracer implements the complete
authorization, negotiation, recovery, and budgeting model described here.

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

Mobile, browser, and native shells remain destination adapters rather than
special protocol roles. Their target layout, geometry, scale, quality, input,
and presentation behavior is specified in
[Remote presentation targets and quality](remote-presentation.md). Dioxus,
Android view, MediaCodec, `AHardwareBuffer`, EGL, and wgpu types remain behind
those adapters.

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

An XR workspace handoff may deliberately admit every eligible window in one
workspace or desktop scope. Snapshot admission moves the current set; follow
scope also admits later matching windows. The broad scope is one authorized
session containing independent window presentations, not a flattened desktop
video stream. A sliding layout, infinite canvas, or spatial arrangement is
chosen by the destination and does not alter source window identity or family
lifecycle.

An accepted destination may request a view configuration for each admitted
presentation. Hoist policy projects that preference through the source's
[application view-set
contract](surfaces-and-input.md#application-provided-view-sets--exploration).
A stereo-capable application can therefore switch an included window from mono
to stereo when an XR window, workspace, or desktop handoff begins. Ordinary
applications remain mono, and a required stereo admission waits, degrades with
approval, or fails according to the offer instead of pretending a second eye
exists. When the last stereo target leaves, policy may request mono again; the
acknowledged client commit, not session timing, determines when interpretation
changes.

The owned surface tree, including later popups and subsurfaces, always follows
an admitted toplevel so its interaction remains coherent. Those roles do not
become freely placeable windows. A newly created independent toplevel is a
separate presentation even when follow-family policy admits it.

## Remote protocol boundary — Direction

Hoisting owns scope, admission, authoritative client lifecycle, placeholder
state, reclaim, and failure policy. The separate
[Remote presentation protocol](remote-protocol.md) owns handshake, device proof,
capabilities, transport bindings, native and encoded surface modes, codec and
media selection, stream topology, queue lifetimes, adaptation, and staged
network validation.

[Remote presentation targets and quality](remote-presentation.md) owns
destination viewport, xdg window-geometry crop, visual overflow, extent, scale,
quality disclosure, and enhancement policy. Those mechanisms may change
without changing which windows a hoist admits or how the source recovers them.

[Remote media budgeting](remote-budgeting.md) owns resource reservations,
media priority, workspace and visibility demand, resize scheduling, and
admission recommendations. Hoist policy remains the authority that accepts,
refuses, or seeks approval for those recommendations.

An unsatisfied required media or target capability refuses admission. An
optional degradation is returned to hoist policy for explicit source-side
disclosure and approval rather than being applied silently by a binding or
codec adapter.

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

A placeholder should retain stable descriptive metadata, including the last
known window title, so it remains identifiable after relocation or remote
closure. A future **Peek** action may temporarily reveal the current remote
presentation or a bounded live preview without reclaiming the window, changing
placement, or transferring input ownership. Peek availability, cadence, and
input behavior must remain explicit policy rather than an implicit transport
side effect.

For a whole-workspace handoff, source presentation may replace the individual
placeholders with a session-level **handoff shield** that explains where the
windows are presented and exposes an unconditional local reclaim shortcut. It
may resemble a lock screen, but it is not an authentication or security lock:
the compositor remains active, authorized source-attached keyboards and
pointers may still target the remotely presented clients, and the recovery
shortcut is consumed locally before client delivery or shortcut inhibition.
If the operating session is actually locked, ordinary lock-screen security
policy takes precedence and client input is suspended.

The shield is only a visual aggregation. Source layout slots, per-window
identity, reclaim state, and restoration geometry remain recoverable beneath
it. Peer loss, focus-state loss, or the local reclaim shortcut atomically
revokes remote input routes before restoring local presentation, preventing a
held key, button, or stale remote focus epoch from crossing the handoff.

The destination may request a new logical content size. The source translates
that request into a client configure and streams the eventual committed size;
interactive destination resize may temporarily scale the most recent frame.

## Drag-and-drop and transferred data — Direction

Wayland drag-and-drop is compositor-mediated rather than a private exchange
between two clients. The source client starts a data-device drag with offered
MIME types and actions, the compositor maintains the input grab and target
focus, the destination accepts an offer, and the selected payload is written
through a file descriptor. Weld must preserve that lifecycle independently
from the bytes carried by a selected MIME type.

The same-process loopback does not need a remote filesystem abstraction. Once
ordinary local DnD is implemented, input and focus for a relocated presentation
can route back to its authoritative Wayland surface while Smithay retains the
local offer and file-descriptor transfer. Dragging between a local presentation
and a loopback-hoisted presentation should therefore remain an ordinary local
compositor operation. A drag icon is an owned surface role and must follow the
active presentation without becoming a managed window.

A network hoist cannot forward a Unix file descriptor. It needs a neutral DnD
control flow for drag start, offered MIME types and actions, target enter and
leave, acceptance, drop, cancellation, and completion, plus a separate bounded
byte stream for the MIME payload. Native Wayland and Smithay objects remain in
the source adapter; the transport carries stable seat, session, window, and
surface identities. DnD, clipboard, and general file transfer may share data
streaming machinery, but remain separately authorized capabilities.

Some MIME payloads are already portable bytes, such as plain text or an image.
`text/uri-list` commonly names files in the source machine's filesystem and is
not portable merely because its text can be relayed. A remote implementation
must choose explicit policy per offer: stage or upload files and rewrite the
offered URIs for the destination, grant access through an authorized shared
mount or portal-like mechanism, or reject the offer. Copy is the safe initial
file action. Move must not be advertised until completion, partial failure,
conflict, cancellation, and source deletion semantics are defined end to end.

This boundary belongs below presentation: `weld-client` should be able to
express neutral DnD roles and lifecycle, the Smithay adapter in `weld-core`
should own native grabs and descriptors, and a future hoist transport should
relay control and payload streams. Presentation plugins may render a drag icon
or transfer status, but do not own offers, filesystem policy, or transport.

## Media admission and degradation — Direction

The source determines whether a presentation requires alpha from client-buffer
format and compositor-known opaque coverage. The
[remote protocol](remote-protocol.md#adaptive-and-alpha-media--direction) owns
alpha framing, codec selection, and encoder or decoder session accounting.

If a required alpha, color, extent, cadence, target, or hardware constraint has
no compatible path, admission fails by default. Degrading to opaque, reducing
quality, using software in place of requested hardware, or trimming required
content must be visibly disclosed and explicitly approved at the authoritative
source. A transport binding, codec adapter, or destination cannot silently
weaken the admitted presentation.

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

- Pair device endpoints explicitly and persist revocable cryptographic
  identities. Device authentication is not member or resource authorization;
  use the
  [identity and mesh
  model](identity-and-meshes.md#trust-and-authorization--direction)
  for those decisions.
- Authorize each hoist; treat **Follow scope** admission for a workspace or
  desktop as an explicit broad grant rather than a consequence of launching
  one app.
- Scope forwarded input to admitted windows or an explicit remote seat.
- Negotiate clipboard, audio, file transfer, and gamepad injection as separate
  capabilities.
- Never expose the local Wayland socket, Smithay objects, or unrestricted
  synthetic input to a peer.
- Apply the
  [remote protocol gateway
  boundary](remote-protocol.md#transport-bindings-and-logical-flows--direction)
  without granting a relay hoist authority.
- Require the
  [remote
  protocol](remote-protocol.md#buffer-frame-and-queue-lifetimes--direction)
  to bound media toward recent frames and report exhausted recovery state to
  the authoritative source.
- Trace connection, admission, launch, hoist, and reclaim transitions with
  stable peer and session IDs.

## Open work — Exploration

- Per-application PipeWire audio and clipboard semantics.
- Remote drag-and-drop negotiation, payload relay, file staging, and URI
  namespace policy.
- Workspace identity, filtering, mirroring, and local/remote layout meld rules.
- Destination handoff and whether one hoist may move between peers without a
  source-local round trip.
- Whether a remotely closed or temporarily unavailable family member retains,
  replaces, or relinquishes its authoritative source placeholder.
