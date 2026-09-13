# Surfaces and input

## Current surface model — Implemented

Core exports protocol-neutral surface snapshots; `weld-app` represents mapped
application surfaces and popups in Bevy; and `weld-window` optionally claims
them for default client- or server-decorated presentation. Multiple toplevels,
readable subsurfaces above the toplevel root, XDG popups, scaling,
client-decoration move/resize requests, and precise surface picking are
implemented. See
[Architecture](../architecture.md) for lifecycle and geometry details.

New mapped surfaces are not intrinsically default windows. A policy plugin
claims an application role and builds its presentation, allowing another
distribution to replace the default window policy without touching Smithay.

## Application-provided view sets — Exploration

Some applications can render several synchronized views of one logical
surface, such as left- and right-eye images for stereoscopic presentation.
There is no standard Wayland surface state for that meaning. Weld may expose a
private `weld_view_set_v1` extension, but the protocol-neutral client model must
own the resulting view-set metadata so Wayland, OpenXR, emulator, and future
adapters can provide it without entering window or hoist policy.

The Wayland global is advertised for the compositor lifetime; it is not itself
toggled. A client binds it, associates a view-set object with its ordinary
`wl_surface`, and reports the layouts it can produce. Policy may then configure
that surface between mono and a supported multiview layout. The client can
refuse or select a supported alternative. An arbitrary unmodified client cannot
be forced to produce a second view merely because a destination requests one.

Configuration needs serial-based acknowledgement and double-buffered surface
state. A view-set change becomes current atomically with the `wl_surface.commit`
that contains the matching pixels. Until that commit is applied, Weld continues
interpreting buffers under the previous configuration. This prevents an old
mono buffer from being split as stereo, or an in-flight packed stereo buffer
from being mistaken for mono when a session ends.

The first useful representation is one ordinary buffer containing full-detail
views in declared source rectangles, initially mono, side-by-side stereo, or
top-bottom stereo. It preserves existing buffer ownership, release, damage, and
explicit synchronization and can use one encoder session. Independent buffers
per view require additional atomic commit and release semantics and should wait
for a concrete case that cannot use a packed view set.

Several simultaneous presentation targets contribute desired view
configurations. Policy selects one client configuration that satisfies the
accepted targets where possible: if any accepted target requires stereo, the
client may produce stereo while mono targets select one view or a defined mono
projection. Incompatible requirements require another rendition or explicit
degradation rather than rapidly toggling the client for each consumer.

View-set capability distinguishes **fixed** views from **tracked** views. Fixed
views declare a stable semantic relationship and can be committed whenever the
application content changes. Tracked views additionally accept a view request
with a session-relative reference space, predicted presentation time, request
identity, per-view pose and field of view, recommended extent, and application
transform. Enabling stereo and supplying a tracked render request are separate
operations: the former changes persistent surface interpretation, while the
latter describes one time-sensitive frame.

A tracked commit identifies the request used to render it and reports the
actual rendered view metadata. Core can then reject a mismatched generation,
discard a superseded result, or forward it for destination reprojection without
guessing which pose its pixels represent. Fixed-view clients need none of this
per-frame tracking state. A target that requires tracked views must explicitly
degrade or refuse an application that advertises only fixed stereo.

Optional alpha or depth belongs to the same synchronized frame group as color,
even if an adapter transports the planes separately. Pixel transparency never
defines the surface input region. The client declares input independently, and
policy decides whether a spatial presentation is interactive.

## Window management — Direction

Application windows should share stable policy components for placement,
stacking, workspace membership, output membership, focus, constraints, and
visual state. Distinct Wayland roles such as popups and layer surfaces must not
be disguised as ordinary windows merely to reuse code.

Protocol roles and their surface-tree ownership remain part of this boundary.
Stable frames, persistent vacancy, and floating or tiling policy are specified
separately in [Window management](window-management.md).

When layer shell becomes a concrete slice, Smithay's `LayerMap` should handle
protocol anchors, margins, exclusive zones, and configure state before Weld
projects the committed result into application policy.

## Current input path — Implemented

Nested winit input and DRM/libinput input enter through separate adapters,
then share timestamped, seat-aware raw records. Reserved compositor shortcuts
are filtered synchronously; every other event is delivered to the focused
client at input pace through Smithay and retained for lossless Bevy/Leafwing
projection on the next refresh-paced application frame. Bevy picking publishes
the client layer and coordinate transform that core retains between frames.
The nested adapter respects the host compositor's logical key mapping; the
direct backend owns its keymap. DRM cursor motion can update presentation
immediately from the completed composition, while also requesting one
refresh-capped application composition for Bevy/Leafwing input and picking.

Standalone DRM touchpads use clickfinger when the device supports it, mapping
one, two, and three-finger physical clicks to left, right, and middle buttons.
Tap-capable devices also enable one, two, and three-finger left, right, and
middle taps. Tap-and-drag, drag-lock, and disable-while-typing retain their
libinput defaults.
Libinput swipe, pinch, and hold gestures are exposed as full-fidelity Bevy
messages and forwarded to clients through the Wayland pointer-gestures
protocol. Gesture consumption is not implemented yet, so compositor plugins
cannot currently claim a gesture and suppress delivery to the focused client.
DRM session loss cancels active gestures and finger scrolling before clearing
focus. Per-device cancellation on hot unplug remains future work alongside raw
device identity.
The nested Linux backend does not receive equivalent gesture events from
Winit; finger scrolling remains ordinary axis input rather than being guessed
into a higher-level gesture.

## Pointer capture and relative motion — Direction

Support application-requested pointer locking and confinement as general input
capabilities, independently from ordinary button grabs and compositor window
move/resize interactions. The motivating report is Blender's numeric-value
drag: holding LMB hides and locks the cursor while motion continues changing
the value without hitting desktop edges. Confirm the exact client protocol
requests when implementing; this is a recorded gap, not an implemented feature
or a confirmed Blender-specific defect.

Use Smithay's relative-pointer and pointer-constraint support at the Wayland
adapter boundary. Preserve accelerated and unaccelerated motion deltas and
timestamps from input adapters rather than reconstructing them from absolute
positions, output traversal or clamped coordinates. Cursor visibility, locked
versus confined motion, constraint region and position hints are distinct state.
Ordinary LMB release must continue reaching the client while capture is active.

The protocol-neutral client/seat contract must express capture requests and
activation/revocation outcomes. A focused authorized client may request capture;
the presentation/input host grants or denies it and always retains a local
escape. Nested presentation requires cooperating with its host compositor,
not merely stopping updates to Weld's own cursor image.

Hoisting must forward the request to the device supplying pointer input and
return its activation state to the source client. Scope it to the authorized
seat, surface and focus/session epoch; reject delayed requests after focus
changes. On focus loss, surface destruction, reclaim, disconnect, VT/session
loss or emergency escape, release the active constraint, restore appropriate
cursor visibility/location and reconcile held buttons. Persistent requests may
reactivate only under the client protocol's lifetime rules and fresh policy
authorization. A destination that cannot provide capture must report that
limitation, not pretend hiding the cursor supplies unbounded relative motion.

See [remote protocol](remote-protocol.md) for transport-neutral input records.

## Seats and devices — Direction

A seat is a logical collection of input capabilities, not a synonym for one
physical keyboard or mouse. Multiple devices may feed one seat; independent
users generally require separate seats. Multiple visible cursors on one seat
are a separate policy feature and must not be inferred merely because multiple
pointers are connected.

Core input state should be keyed by stable logical seat identity. Each seat
owns its keyboard focus, pointer focus, cursor, pressed keys and buttons,
modifiers, gesture sequences, grabs, constraints, and monotonically ordered
focus epoch. Input records name both the seat and the focus epoch they were
routed under, preventing delayed motion, releases, or remote packets from being
reinterpreted after focus changes.

Focus is exclusive within one seat, not globally across the compositor.
Different seats may focus different surfaces simultaneously, and several seats
may focus the same surface when policy and the client support it. A physical
device is assigned to one logical seat at a time; local, nested, remote, virtual,
accessibility, and replay adapters all enter the same seat contract rather than
creating special focus paths.

Identity and permission remain outside the seat primitive. A collaboration or
remote policy may map an authenticated participant and its authorized devices
onto a seat, restrict that seat to selected surfaces, grant view-only access, or
require an exclusive control token. Core enforces the resulting input route and
revocation but does not treat network peers, mesh members, operating-system
accounts, and seats as interchangeable identities.

Wayland clients that bind several `wl_seat` globals can receive genuinely
independent input streams. Client support cannot be assumed: distributions need
an explicit compatibility mode that grants one controlling seat while other
participants remain view-only or request handoff. XWayland and other
single-focus adapters may require that fallback even while native clients use
multi-seat input elsewhere.

Client keyboard or pointer focus is also distinct from window activation,
selection, raising, and visual emphasis. Window-management policy may select a
primary active seat for conventional single-active clients and decorations, or
show per-seat focus indicators, without collapsing the underlying seat focus
routes into one global value. A focus request never implicitly grants input
authority or changes stacking.

Input policy should support click-to-focus, focus-follows-pointer, directional
focus, focus history, grabs, pointer constraints, shortcut inhibition, touch,
tablet input, and global-shortcut protocols without exposing backend-specific
events to plugins. Remote input must stay scoped to its authorized window or
explicit remote seat.

### Input producers and remote control

Presentation ownership must not determine where physical input devices live.
A source-attached keyboard or mouse may deliver directly to the source's
authoritative client while following the destination's accepted focus selection
and the seat assignment above. It must not bounce through the headset and back,
or route by the source placeholder's visual selection. Destination-attached
devices enter the same contract through ordered hoist input. Source policy
validates focus requests; attention and an unaccepted focus candidate are not
authority to receive input.

The first mixed-device experiment may have a laptop keyboard/mouse and headset
controller contributing to one logical seat. Track held keys, buttons and
virtual modifiers per producer before aggregating seat state: one producer's
release must not cancel another's hold. Keep terminating events tied to their
original interaction, using the focus/grab and revocation rules above. Do not
create another focus or pressed-state implementation inside each shell.

An input-only control window is a candidate source adapter when another
compositor, such as Sway, owns the desktop. It receives input while deliberately
focused/captured, identifies the controlled application, and provides a local
escape/reclaim action. It is not a video presenter and not an operating-system
security lock screen. It must not imply global interception of another
compositor's input or claim capture that the host refused. When Weld owns the
desktop, its native input adapter can supply the same route without that window.
Capture/focus loss follows the existing
[cleanup contract](#pointer-capture-and-relative-motion--direction).

Reuse [explicit keyboard repeats](../keyboard-repeat.md). Current repeat
ownership is seat-wide and chosen at startup; automatic arbitration between
heterogeneous controllers is not implemented. Supporting mixed producers needs
an explicit compatible cadence policy, not a mode switch on each event or a
second hidden repeat timer. Preserve balanced presses/releases and invalidate
repeat for a revoked hold. Virtual-keyboard taps must not become indefinitely
held physical keys.

Physical key transitions and text/IME composition are different capabilities.
An adapter must not reinterpret a Unicode character as a native keycode or
assume autocorrection/preedit is a sequence of physical key presses. Similarly,
touch-to-pointer translation is not native touch: real touch needs contact
identity, down/move/up/cancel and frame semantics. Shell navigation remains
separate from application input and must consume its complete gesture once
claimed; the current gesture-consumption gap is not resolved by this Direction.

## Open work — Exploration

- Define the smallest transport-neutral view-set metadata and the corresponding
  optional Wayland configure, acknowledgement, and commit state machine.
- Define tracked view-request and frame-group records, including relative
  reference spaces, pose/FOV acknowledgement, color/alpha/depth synchronization,
  and stale-request handling.
- Multi-output workspace and focus behavior.
- Define stable logical-seat and focus-epoch records, device assignment,
  revocation, and per-seat cursor projection.
- Validate native multi-seat clients and explicit single-controller fallbacks
  for clients or adapters that assume global focus.
- Decide whether a distribution wants multiple cursors per seat in addition to
  the ordinary one cursor per independent seat.
- Touch parity for client move and resize.
- Stable public layout and focus extension contracts.
