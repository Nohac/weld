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

## Cursor configuration and application runtimes

Master's `seat * xcursor_theme THEME [SIZE]` configures the session cursor theme.
Weld publishes the name and size through `Gtk/CursorThemeName` and
`Gtk/CursorThemeSize` XSettings, the X11 `Xcursor.theme` and `Xcursor.size`
resources, and `XCURSOR_THEME` / `XCURSOR_SIZE` for newly launched applications.
Cursor configuration is applied before startup clients and before launch commands
from the same policy update. Reload updates the X11 settings and Weld's own
cursor rendering. Existing processes retain their launch environment, and
application cursor caches may require an application restart.

Selecting a cursor theme also exposes its installed artwork through
`$XDG_DATA_HOME/icons/THEME` (normally `~/.local/share/icons/THEME`). Weld uses
the `xdg` crate to resolve the data directory and `xcursor` to locate the selected
theme's own default cursor. Existing files, directories and user/package-managed
symlinks, including dangling links, take precedence and are left unchanged.

Weld-created public links point to `../weld/cursor-themes/THEME/current`.
That managed directory records the original installation path as `source` and
its resolved directory as `current`. Selection on a subsequent startup or theme
change refreshes `current` atomically, allowing a stable package/profile path to
follow an upgraded installation. Updates are serialized across Weld instances.
Resolution failures are logged, and the installed artwork is left untouched.
Themes whose default cursor comes entirely from an inherited theme
are left to normal theme discovery.

Changing themes or shutting down normally releases the session's managed link.
Shared file locks retain a theme while another Weld session has it selected; the
last session removes the public link and its managed theme metadata. The shared
update-lock file remains for coordination. Cleanup verifies that the public link
still points to Weld's managed entry, preserving entries replaced by another
tool. Cached cursors in existing apps may remain until the app reloads them.
Forced termination can leave links behind; selecting that theme again with an
available source adopts its managed lifetime and cleans it on the next release.
Applications still need access
to the underlying artwork within their runtime filesystem.

XSettings communicates settings; applications still load the cursor artwork
from their filesystem. Xcursor searches theme directories along its configured
search path and falls back to inherited/default themes when artwork is missing.
See the [XSettings specification](https://specifications.freedesktop.org/xsettings/latest/)
and [Xcursor theme lookup documentation](https://xorg.freedesktop.org/archive/X11R7.5/doc/man/man3/XcursorSetTheme.3.html).

### Steam runtime theme visibility on NixOS

Investigation on 2026-10-11 found that Steam continued showing Bibata Modern Ice
while Weld was configured for Bibata Modern Amber at size 50, even after restarting
both applications. The observed boundary was Steam's Linux Runtime container
around `steamwebhelper`:

- Steam and its web helper both received `XCURSOR_THEME=Bibata-Modern-Amber`
  and `XCURSOR_SIZE=50`.
- Weld's X resource database and XSettings contained Amber. A GTK3 query against
  Weld's X11 display also resolved Amber at size 50.
- The host could read Amber under `/run/current-system/sw/share/icons`, but
  that path was unavailable inside the web helper's runtime, despite appearing
  in its `XCURSOR_PATH`.
- Ice was available through the user's icon directories. Amber's underlying
  Nix store files were accessible, but had no reachable entry in the runtime's
  cursor search directories.
- Adding `~/.local/share/icons/Bibata-Modern-Amber` as a symlink to the installed
  theme made its cursor files readable from the existing web-helper container.
  File visibility was verified directly, and the user subsequently confirmed
  that the theme worked. Automatic link creation and cleanup were also manually
  verified after implementing the managed-link lifecycle.

This evidence identifies theme-file discovery as the remaining failure after
settings publication was corrected. It also explains why querying a theme name
successfully is insufficient to validate the application's rendered cursor.

A [matching NixOS Steam issue](https://github.com/NixOS/nixpkgs/issues/437281)
reports that a theme in `environment.systemPackages` can be unavailable to Steam.
Its reported declarative workaround is to include the theme in Steam's environment:

```nix
programs.steam.extraPackages = [ pkgs.bibata-cursors ];
```

The [NixOS Steam module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/programs/steam.nix)
provides `extraPackages` for this purpose. The local investigation used the
per-user symlink workaround; the declarative alternative still needs validation
on this installation. Weld now automates exposure for missing user-theme entries
as described above. The original manually created Amber link remains user-owned;
automatic exposure leaves it untouched. Neither approach changes the selected
global GTK theme.

For similar reports, check the settings received by the affected process and
whether that process can read the selected theme along its actual search path.
Containerized helpers may have a different filesystem view from their launcher.
Runtime/package integration determines access to theme files; Weld's user-data
links cover runtimes that can read those directories and the resolved targets.
Weld's shared cursor policy can additionally explore
[semantic recognition of image-only cursors](spec/surfaces-and-input.md#semantic-cursor-recognition--exploration).

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
