# Desktop layer surfaces

Weld advertises `wlr-layer-shell` for native Wayland panels and launchers.
Waybar and Rofi share the ordinary surface-buffer, input-region, cursor,
scale and presentation-callback machinery.

## Ownership

Core tracks layer roots separately from XDG toplevels. Smithay's per-output
`LayerMap` arranges anchors, margins, exclusive reservations and configures.
Initial configuration follows the first surface commit; a null-buffer unmap
releases the reservation, and remapping follows a fresh configure cycle.
An explicit output selects that output; an omitted output selects the primary.

`ClientSurfaceRole::Layer` carries committed output-local placement, composition
layer, keyboard interactivity and stack order. `weld-app::layer_shell` projects
mapped roots and their XDG popups through `SurfaceNode`. Each presentation has
a layout root with a separate content child: `SurfaceNode` owns its drawing/input
children, while popups retain the layer's layout root as their parent. The rendering and
buffer-release path is shared with application surfaces. Desktop roots never
receive `ClientToplevel` or enter ordinary managed-window admission.

Core separately publishes `OutputWorkArea`, the remaining logical rectangle.
The tiler fits its workspace bounds and gaps into that rectangle. Output pixel
size, scale, topology and mode remain unchanged. Destroying the final panel
restores the full output work area.

Mapped top/overlay surfaces requesting exclusive keyboard access temporarily
override ordinary window focus. On-demand surfaces acquire focus when clicked;
noninteractive panels leave keyboard focus alone. Ordinary focus requests keep
the underlying selection current. Closing or unmapping the launcher restores
that selection through the normal client request path. Compositor shortcuts
remain available.

Source hoist admission rejects desktop roles and owner-related popups. A manual
map requested before its role arrives waits for that role before admission.
The receiver rejects transported layer roles as well. Whole-output screenshots
include desktop layers because they capture the completed composition.

## Validation

Run the isolated nested smoke test with installed `foot`, `htop`, `waybar` and
Wayland-enabled `rofi`:

```sh
python3 scripts/check-layer-shell
python3 scripts/check-layer-shell --no-build --interactive
python3 scripts/check-layer-shell --no-build --workspaces --interactive
```

The runner uses private Waybar settings, retains before/panel/launcher/after
captures under `target/validation/layer-shell-*`, and stops its own processes.
The interactive variant leaves 90 seconds for typing in Rofi, Escape, and
checking restored htop input. An updating terminal drives the existing
frame-serviced remote-debug endpoint. Debug ports 15702 and 15703 must be free.
The `--workspaces` variant uses the example's startup Waybar. After dismissing
Rofi, use Alt+2 and Alt+Enter to populate another workspace, click between them
on the bar, and try Alt+Space to reopen Rofi. Alt+Shift+R should retain one bar.
Logical Alt is the physical Windows key with the example's configured swaps.

Native protocol regression tests require a valid `XDG_RUNTIME_DIR` but no GPU:

```sh
cargo test -p weld-core --features test-support layer_tests --locked -- --ignored --test-threads=1
```

They cover initial configure ordering, reservation release, launcher focus,
frame callbacks, remapping, destruction, selected-output membership and popup
scale propagation. ECS and relay tests cover work-area reflow, role separation,
temporary focus arbitration and rejection from hoisting. The nested visual test
has verified Waybar placement, Rofi overlay presentation and restored layout.
Manual testing confirmed typing in Rofi and focus restoration. It also exposed
the need to reassert an unchanged launcher selection on click after nested host
focus loss; a pointer-observer regression test covers that path.

## Follow-ups

Sway IPC and foreign-toplevel enumeration for Rofi window switching are subsequent slices.
This slice targets Rofi application launching. Layer popup geometry applies the
client's flip/slide/resize constraints against its output on initial configure
and reposition. Bounds are converted to parent coordinates, including panel
placement, output scaling, and nested-menu offsets.
Dynamic physical output hotplug and floating-window placement within reserved
areas remain separate work. A layer surface is not a secure session lock.

## Workspace controls

Master enables `weld_window::workspace_protocol::WorkspaceProtocolPlugin`.
It projects existing workspace identities, names, output membership and visibility
through `weld-app` to the core host. Change detection skips unchanged WM frames;
the native adapter diffs snapshots and emits complete `ext-workspace-v1` updates.
The global appears after the first policy publication. Session-stable IDs remain
internal rather than claiming the protocol's persistent cross-session identity.

One group represents each output. Workspaces on every output are advertised;
inactive workspaces remain eligible for display in switchers. The supported
request capability is activation. Requests wait for the client's manager `commit`,
then run through the same i3 switch policy as keyboard bindings, preserving
per-output visibility and remembered window focus. Inventory publication follows
the complete policy update. Stale requests are ignored. Pending requests and
committed transactions are bounded. Group creation, workspace creation/removal,
deactivation and reassignment are not advertised as client capabilities.

Waybar should use `ext/workspaces`, as the included example does; `sway/workspaces`
requires Sway IPC. Rofi `drun` launches applications; its window switcher needs a
separate foreign-toplevel protocol implementation.

Regression checks:

```sh
cargo test -p weld-i3-quirks --locked
cargo test -p weldwm --lib master:: --locked
cargo test -p weld-core --features test-support workspace_tests --locked -- --ignored --test-threads=1
```

The wire tests cover initial inventory, unchanged snapshots, rename/state/output
changes, commit batching, stale requests, handle release, late output binding and
leave-before-removal ordering. Policy tests cover activation/focus restoration;
configuration tests cover initial execution, reload selection and invalid reloads.
The nested test confirmed startup Waybar, Rofi's configured launcher shortcut,
workspace creation from bindings and click-to-switch between populated workspaces.
Reload/no-duplicate-bar behavior has automated coverage; it was not manually
exercised in that run.
