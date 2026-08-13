# Window management

## Current boundary — Implemented

The first policy-neutral split is implemented. `weld-window` owns durable
managed windows and optional client occupancy independently of UI;
`weld-window-ui` supplies reusable unstyled Bevy UI presentation behavior;
`weld-ssd` supplies the current opinionated decoration scene; and `weld-float`
supplies default freeform management policy. A client surface can disappear
without despawning a managed window whose vacancy policy is `Retain`.

The exact implemented ownership and dependency direction are documented in
[Architecture](../architecture.md). Persistent matching, workspaces, native
tiling, and persistence below remain direction rather than completed features.

## Managed frame model — Direction

Window-management consumers should operate on stable **managed frames**, not
directly on the shorter-lived client surfaces that may occupy them:

- A **client toplevel** and its owned surface tree represent the live protocol
  object and client content. Popups and subsurfaces remain attached according
  to their Wayland roles rather than becoming independent frames.
- A **managed frame** represents stable layout and policy identity, including
  geometry, workspace and output membership, constraints, manager selection,
  and presentation state.
- An **occupant** is an optional client toplevel attached to a frame. Replacing
  or detaching it does not inherently replace the frame.

Frames, occupants, layout containers, matching state, manager selection, and
presentation state should be owned Bevy ECS state. Plugins operate on them
through entities, components, relationships, resources, systems, and typed
commands or events as appropriate. Host ingress projects owned,
protocol-neutral state into ECS; validated typed effects carry requested
protocol actions back to the host. The wider Smithay, rendering, and identity
constraints remain those in
[Core runtime](core-runtime.md#ownership-rules--direction): in particular,
Bevy `Entity` values stay process-local while persistent and external
boundaries use stable Weld-owned IDs.

This direction does not require one final component schema before an
implementation slice proves it. It does require window policy and layout to be
observable and replaceable ECS state rather than private state hidden inside a
single manager plugin.

A vacant frame remains a complete window-management object. Consumers can
select, move, resize, tile, reparent, assign, or remove it without inventing a
fake Wayland surface. Selecting a vacant frame updates manager-level focus
history and keeps compositor bindings available, but establishes no Wayland
keyboard focus and delivers no client input. Client-only actions require an
occupant. Closing or detaching an occupant and removing its frame are distinct
operations.

Occupancy and local presentation are separate state axes:

| Frame state | Client occupant | Local presentation |
| --- | --- | --- |
| Ordinary local window | Present | Client content and chosen decoration |
| Vacant persistent frame | Absent | Compositor-owned vacant presentation |
| Hoisted window | Present at source | Compositor-owned remote/reclaim presentation |

The last state preserves the source client while moving its interactive
presentation elsewhere. It must not be modeled as vacancy merely because the
source no longer draws the client texture locally.

## Persistent frames — Direction

A **persistent frame** is an opt-in managed frame that survives the loss of its
occupant. Its layout position remains reserved, and a later matching client
toplevel can attach to it without shifting the surrounding layout.

The primary initial use case is application development. Bevy, Godot, and
other frameworks may destroy and recreate a player window on every run or
recompile. A persistent frame lets each replacement return to the same
floating geometry or tiling position rather than disturbing the workspace.
The same managed-frame mechanism may later support saved layouts, launch
reservations, client crash recovery, and hoist placeholders without creating
separate kinds of layout object.

An interactive binding should be able to make the selected frame persistent
and propose criteria from its current occupant. The user can choose or edit a
combination of Wayland application ID, X11 class where applicable, and title.
Application ID or class is normally more stable than title. The manager must
make ambiguity policy explicit when several toplevels match, such as attaching
the first eligible window, asking the user, or creating additional frames.

Configuration should also support rules that automatically make matching
frames persistent. The exact configuration syntax is not selected; an
illustrative rule could express:

```text
persistent-frame "game-preview" {
    match app-id = "^ExampleGamePlayer$"
    match title = "^Example Game Preview$"
    workspace = 3
    floating = true
}
```

A rule may reserve a vacant frame in advance or mark a claimed frame persistent
when its first occupant appears. Runtime-created frames may optionally persist
across Weld restarts using stable Weld IDs and serialized project-owned state,
never Bevy entities or native protocol objects.

Matching is evidence for placement, not authority. It must not grant remote,
input, launch, or other privileged capabilities, and matching an application
must remain distinct from launching it. The term “persistent frame” also avoids
overloading i3 and Sway's existing “sticky” behavior for floating windows that
appear across workspaces.

## Composable responsibilities — Direction

The initial default window policy is split into independently composable
responsibilities:

- **Window primitives** own the ECS managed-frame and occupant contracts,
  surface claiming, generic interactions, focus/selection, geometry,
  workspace membership, and presentation state. They do not automatically add
  title bars, borders, initial placement, or a particular layout.
- **Server decorations** render borders and title bars with ordinary Bevy
  composition and translate UI interactions into generic frame requests. They
  remain optional and respect client/server decoration policy.
- **Floating or freeform policy** provides conventional desktop placement,
  z-order, move and resize, raise/focus, maximization, and fullscreen behavior.
- **Tiling policy** owns native layout containers and transformations without
  requiring server decorations or i3 compatibility.

“Stacking” denotes z-order in floating policy, while a **stacked container** is
a specific tiling layout alongside split and tabbed containers. It is not used
as the name of the conventional freeform manager.

These labels describe separable plugin responsibilities. They do not require
creating forecast-only crates; a package split should follow an implemented
dependency, runtime, reuse, or testing boundary.

A future tiler should be able to depend on `weld-window` alone. It would claim
a managed frame by attaching its own `ManagedBy` relationship, derive layout
from its ECS container tree, and write the frame's `WindowGeometry`,
`WindowVisibility`, and `WindowZOrder`. It would consume the same
`WindowIntent` stream and issue the same `WindowCommand` protocol effects as
the floating manager. Adding `weld-window-ui` or `weld-ssd` would then be a
separate presentation choice rather than a requirement of tiling policy.

## Native tiling foundation — Direction

The native tiling layer should represent an explicit, queryable layout tree in
ECS. Stable frame and container IDs must remain distinct from process-local
entities. Its model should be capable of representing split, stacked, and
tabbed containers; workspace and output association; floating overlays; and
fullscreen state.

Typed operations should cover deterministic directional focus, moving frames
and subtrees, reparenting, splitting, changing layout modes, and removing or
retaining vacant frames. Related mutations should become visible atomically so
rendering, IPC, persistence, and other plugins do not observe half-applied
trees. State transitions and results should be observable through typed events
or queries instead of requiring consumers to reach into one manager's private
state.

Animation is presentation over a committed layout result, not part of each
layout algorithm. The initial implementation does not need every possible
layout, but its identities, relationships, operations, and observation points
should permit new algorithms without replacing the window primitive.

## i3 and Sway compatibility — Exploration

An i3/Sway-compatible layer could improve adoption, but compatibility is not
an early requirement and should remain separate from native tiling. The tiling
foundation should not parse i3 configuration or expose i3 IPC types as its
internal model.

A future adapter may translate selected compatibility surfaces onto native
typed operations:

- i3/Sway configuration and key bindings;
- criteria and `for_window` rules;
- commands and layout behavior;
- IPC tree queries and event subscriptions; and
- behavioral details required by existing scripts and tools.

Compatibility should be claimed per tested surface rather than as an immediate
all-or-nothing promise. Stable identities, a complete queryable tree,
deterministic operations, and observable transitions are the important early
constraints that keep such an adapter feasible without copying i3's internals
into Weld's general window model.

## Per-window accessibility zoom — Exploration

Weld could provide compositor-owned zoom for an individual managed frame. The
zoom should be ECS state on the durable window primitive rather than behavior
hidden in server decorations, a particular manager, or a client surface. That
would let floating, tiling, remote, and custom presentation plugins expose the
same accessibility capability.

A complete implementation should scale the window's composed presentation as
one unit, including client content, server or client decorations, shadows,
popups, clipping, and input regions. Bevy UI transforms are a candidate for
composition and transformed picking. The host could additionally advertise an
effective preferred buffer scale derived from the output scale and window zoom
to the occupied Wayland surface tree. Clients that honor integer or fractional
preferred scale could then redraw sharply; clients that do not would remain
usable through compositor sampling but may look softer.

The geometry contract needs validation before selecting a component schema.
One candidate keeps `WindowGeometry` as the final desktop footprint and shows
less client-logical content as zoom increases. This remains predictable for
tiling and placement. A manager may separately enlarge that footprint when it
wants to preserve the visible content area. Resize deltas, constraints, popup
placement, surface-local pointer coordinates, clamping, and per-surface scale
lifecycle must all remain coherent under either policy.

Focused-window increase, decrease, and reset shortcuts are a useful initial
interaction, but bindings belong to configurable policy rather than the window
primitive itself. Per-window zoom should compose with output scaling instead
of replacing it.

## Open work — Exploration

- Exact ECS components and relationships for frames, occupants, and layout
  containers.
- Frame claim arbitration and transactional occupant replacement.
- Persistence storage, rule migration, and multiple-match interaction.
- Default vacant-frame presentation and accessibility semantics.
- Per-window accessibility zoom geometry and preferred-buffer-scale policy.
- The smallest native tiling slice that validates the tree and operation
  contracts.
