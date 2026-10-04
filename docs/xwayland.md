# Rootless XWayland

Weld can host X11 windows in its ordinary nested, DRM and headless sessions.
Enable it explicitly for this initial slice:

```sh
cargo run -- --backend nested --config examples/master.sway.config --xwayland -- steam
scripts/run-weld-drm --config examples/master.sway.config --xwayland -- steam
```

`Xwayland` must be installed and on PATH. Close any existing desktop Steam
instance first: Steam's single-instance launcher may otherwise reuse that
instance instead of opening a window inside Weld.

Weld creates its own rootless X server and passes its `DISPLAY`, alongside its
Wayland socket, to launched clients. Toolkit backend discovery can use either
endpoint. Applications started through config `exec`, Rofi or a terminal inside
Weld inherit those endpoints. The external `scripts/run-app` helper still selects
native Wayland only; it does not discover the X11 display of another Weld process.
No process-global desktop `DISPLAY` is changed, and TCP X11 listening is disabled.

## Shared pipeline

Smithay's `X11Wm` and `XWaylandShellState` translate X11 control events and associate
each X11 window with its Wayland surface. `WindowSurface` encapsulates XDG/X11
close, activation and resize behavior. The ordinary native surface registry,
buffer leases, DMA-BUF synchronization/import, commit coalescing, presentation
claims, frame callbacks and hoist adapters handle the content.

Surface-tree sampling receives window geometry from the native window adapter.
XDG geometry and X11 client-frame extents use the same crop/input machinery.
X11 desktop placement uses `last_configure`; surface-local geometry is used for
the visible content rectangle. Keyboard targets retain X11 input-model negotiation
through Smithay while pointer events use the associated Wayland surfaces.
The selected X11 input target is raised in XWayland before pointer delivery;
an active pointer grab retains its original target. This uses surface-local
input and keeps remote/XR window placement under the presenter's ownership.
Cursor frame callbacks complete when Weld consumes the cursor buffer, including
across focus changes, so XWayland can submit subsequent cursor shapes.

The existing calloop loop dispatches directly against compositor state.
Backend notifications accumulate in a same-thread queue and are drained by the
existing native drivers. There is no additional render clock or video worker.

Normal X11 windows publish ordinary toplevel roles with minimum/maximum size
hints and generic dialog/splash/utility/toolbar classification. Master centers
unparented special or fixed-size windows in the selected output work area;
explicit transients center over their parent. Transients publish parent
relationships. Override-redirect menus publish popup roles relative to a resolved
toplevel; nested transient menus resolve to that root. For missing transient
hints, Weld uses a matching application's pointer/keyboard target and preserves
that association. Unresolved popup ownership waits for a usable relationship.
Closing an owner retires its published popup dependents.

Client identities use XRes-reported local process IDs to group windows, with
separate identities when that information is unavailable. Thus unrelated apps
sharing XWayland's Wayland connection are not automatically one hoist family.
Title, class and instance metadata are published only when changed; X11 class supplies the
neutral application-label field, with instance preserving the X11 origin for
configuration criteria. These hints provide grouping, not a
security boundary: clients sharing an X server retain X11's mutual access.

An unmap retires the associated surface. Remapping gets a new surface identity
and enters normal window/hoist admission. Startup failure is logged and closes
the XWayland connection; asynchronous readiness has a ten-second deadline.
XWayland disconnection retires its windows while native Wayland hosting continues.
Automatic XWayland restart is a follow-up.

## Validation

The Rust probe draws an animated bar and input counter using X11 commands.
The runner builds the normal debug executable and probe, launches a bounded
session, captures settled content and cleans up its process groups. It disables
core dumps and keeps logs under `target/validation/xwayland-*`.

```sh
python3 scripts/check-xwayland
python3 scripts/check-xwayland --no-build --hoist
python3 scripts/check-xwayland --no-build --hoist --lifecycle
python3 scripts/check-xwayland --no-build --hoist --interactive
```

`--hoist` uses headless source, nested receiver and the existing H.264/Iroh path
with private peer exchange and same-device direct networking. `--lifecycle`
maps a transient dialog and override-redirect popup, destroys them, unmaps/remaps
the main window, and captures both phases. `--interactive` leaves 90 seconds for
input, resizing and close checks. Debug ports 15702 and 15703 must be available.
An optional command after `--` replaces the probe. The test requires an actual
X11 window association, so silently using native Wayland does not pass.

Accelerated example, with an unavailable Wayland endpoint to select Blender's
X11 backend:

```sh
python3 scripts/check-xwayland --no-build --hoist -- \
  env WAYLAND_DISPLAY=weld-intentionally-unavailable blender --factory-startup -noaudio
```

Verified so far: live probe animation locally and over Iroh, transient/popup
presentation and retirement, root remapping, and accelerated Blender presentation
over XWayland/Iroh. Buffer diagnostics confirmed the X11 drawing probe arriving
as DMA-BUFs. Existing native layer/output/workspace protocol tests continue to
pass after the keyboard-target and shared-geometry changes. Steam's main Library
UI and interaction were confirmed through the same Iroh path, and the user
successfully launched a game and verified the cursor-shape fix. This is one
game-launch smoke test, not broad game compatibility coverage.

`RUST_LOG=info,weld_surface_diag=trace` records import kind and frame activity;
use it only for bounded diagnostic runs. The existing nested Vulkan acquisition
fence validation messages also appear in these tests.

## Remaining compatibility work

Relative motion and pointer position are delivered together, keeping XWayland's
motion and wheel input on the same virtual device. This prevents Chromium from
discarding wheel samples after repeated XInput2 device switches. Steam wheel
scrolling and hover feedback have been confirmed in a physical DRM session.
The X11 root cursor uses Weld's shared cursor theme and follows configuration
updates; client-defined cursor artwork remains application-owned.

The WM's fullscreen/minimize policy, pointer locking and confinement, X11/Wayland clipboard
and drag-and-drop bridging, X11-specific scale policy, resize increments/aspect
hints, and application activation requests need separate validation or support.
Floating resize constraints cover minimum/maximum dimensions; tiled/fullscreen
requests override them. Tiled X11 clients receive both maximized flags, matching
[Sway's XWayland policy](https://github.com/swaywm/sway/blob/1.12/sway/desktop/xwayland.c).
WM-requested close
still needs a dedicated interactive check. X11 root-coordinate bounds require
windows to fit the advertised output space; larger remote presentations need
additional virtual-output sizing policy. X11 restacking and Wayland input use
different sockets, so strict first-event ordering needs further validation.
Broader game compatibility and mixed-output X11 scaling still need validation.
