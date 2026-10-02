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
mapped roots and their XDG popups through `SurfaceNode`. The rendering and
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
```

The runner uses private Waybar settings, retains before/panel/launcher/after
captures under `target/validation/layer-shell-*`, and stops its own processes.
The interactive variant leaves 90 seconds for typing in Rofi, Escape, and
checking restored htop input. An updating terminal drives the existing
frame-serviced remote-debug endpoint. Debug ports 15702 and 15703 must be free.

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

Workspace reporting/activation for Waybar (`ext-workspace-v1`), Sway IPC, and
foreign-toplevel enumeration for Rofi window switching are subsequent slices.
This slice targets Rofi application launching. Layer popup geometry follows the
client's positioner; output-edge unconstraining remains shared follow-up work.
Dynamic physical output hotplug and floating-window placement within reserved
areas remain separate work. A layer surface is not a secure session lock.
