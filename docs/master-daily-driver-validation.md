# Master daily-driver milestones

This phase extends the existing tiling-first Master distribution. The user's
Sway config guides priorities; regression fixtures remain repository-local.

## Milestones

1. Mixed floating and tiling: declared dialogs, mode toggle, remembered geometry,
   workspace transfers, focus-mode selection and configurable pointer modifiers.
2. Configurable frames: titlebar-free pixel borders, sizes, colors and live reload.
   This supplies the borderless presentation needed by fullscreen.
3. Fullscreen: application state negotiation, reversible output-sized geometry,
   ordinary layer suppression and explicit exclusive-output policy. Exclusive
   mode preserves compositor escape shortcuts; direct scanout is an independent
   optimization, not a prerequisite.
4. Mouse-driven tile resizing: shared input capture with adjacent split-ratio edits.
5. Remaining tightly related config gaps: smart gaps/borders and the controls
   needed to exercise these features. Broader SwayFX effects, arbitrary Sway IPC,
   and an entire additional layout implementation are separate projects.

Each implemented milestone gets focused regression tests, independent code
review and its own commit. Physical DRM interaction is left for the user's
return; bounded nested runs and deterministic policy tests run during development.

## Open daily-driver observations

### Rare apparent missing keyboard input in Foot (2026-10-04)

The user reports occasional apparently dropped Enter presses and other keys in
local Foot under the DRM backend, without hoisting. It is rare and has no
reliable reproduction. Whether the input is actually absent or its resulting
repaint is delayed remains unknown; no cause or fix has been established.

The initial code inspection found ordinary key presses/releases forwarded before
paced application updates, with coalescing limited to pointer motion. The repeat
guard filters repeated events rather than ordinary presses. Available logs lack
per-key delivery evidence. The recent hidden-window callback-demand changes are
an investigation candidate, not an established regression.

On recurrence, check whether pointer movement or a later redraw reveals the
missing change. Then correlate shortcut consumption and keyboard focus/routing
with client commits, callback demand and presentation. Any diagnostic key trace
should be opt-in and restricted to a controlled test, avoiding ordinary typing.

## Launching

From the repository root, in the development shell:

```sh
cargo run -- --backend nested --xwayland --config examples/master.sway.config -- foot
```

For a physical TTY:

```sh
scripts/run-weld-drm --xwayland --config examples/master.sway.config -- foot
```

In the example keymap, **physical Windows is logical Alt** and is the modifier
used below. The parent compositor can still consume nested shortcuts, so use
DRM for the final input check. These commands use the development build.
`Control` is physical Caps Lock with the example's explicit XKB swaps.
Launch test apps from Weld's terminal so they inherit its Wayland/XWayland
environment. Quit an existing Steam instance first, and use a separate browser
profile if an existing instance would redirect the launch to the host desktop.

## Floating validation

1. Open three terminals with Mod+Enter. Select the middle one, press
   Mod+Shift+Space, and check that the other two expand into its former space.
2. Mod+left-drag moves the floating window. Mod+right-drag resizes from the
   corresponding corner. Release the button outside the original bounds;
   moving the pointer afterward must not keep dragging.
3. Toggle back to tiled, then floating again. The tile should return to its
   previous split when that split survives; the freeform position/size should
   return on the next toggle.
4. Mod+Control+P switches between the recent tiled and floating selections.
   Close the floating window with Mod+Shift+Q and check focus restoration.
5. Move a floating window with Mod+Shift+2, switch with Mod+2, and verify it
   remains floating. Switch away/back and check visibility and selection.
6. Open an application dialog that declares its parent. It should float centered
   over the parent without shrinking the parent tile. Menus/tooltips should keep
   their protocol positions and disappear when dismissed.
7. Change `floating_modifier` in a private copy of the example and reload with
   Mod+Shift+R. Only the new modifier should initiate a drag. Reloading while
   holding a captured button must still consume its eventual release.
8. Hoist and reclaim both modes. The source's persistent window identity and
   workspace slot should survive; ordinary client input should still work.

## Automated evidence for the floating milestone

- Policy tests cover slot restoration, retirement of emptied splits, parent
  centering, freeform behavior without taking workspace ownership, mode focus,
  cross-workspace transfer, and pointer-binding replacement.
- The existing resize fixture now simulates the client commit required to settle
  a configure. Its former failure was reproduced against the checkpoint source.
- `scripts/check-layer-shell --no-build` completed Waybar/Rofi mapping, capture,
  teardown and restored layout in `target/validation/layer-shell-hibj20lu`.
- The existing X11 lifecycle probe completed dialog/popup map, unmap/remap and
  shutdown in `target/validation/floating-smoke-xhwx9itj`; its capture shows the
  dialog centered over the full-sized parent. The temporary harness lives outside
  the repository; the reusable probe is `weld-core/examples/x11_window_probe.rs`.
- These nested debug runs still reported Vulkan acquisition-fence validation
  messages also recorded before this phase. No warning-free renderer claim is made.

The sections below cover the remaining implemented milestones.

## Decoration validation

1. With two tiled terminals, each should have a narrow pixel border and no titlebar.
   Floating a window should select the normal floating frame with close/move controls.
2. Mod+B cycles pixel, none and normal. Content must remain correctly aligned;
   a floating window should retain its client-content size as chrome changes.
3. With `border none`, there should be no shadow or invisible resize handles.
   Modifier dragging/resizing should still work for a floating window.
4. In a private config copy, change border widths, `corner_radius`, and the
   focused/unfocused palettes, then reload. Existing non-overridden windows
   should update; explicit per-window border selections should remain intact.
5. Open a menu, then change the parent's border style. Popup position and input
   should follow the new content anchor rather than the old titlebar offset.
6. Check a client-decorated app too: these settings must not crop or duplicate
   application-owned titlebars. Recheck opaque hoisted content, which needs Weld's frame.

The example keeps Mod+B for border toggling; launch Blender with Mod+Shift+B.

## Hover focus and split hints

1. Open two terminals and move between their contents and borders without clicking.
   Keyboard input should follow the hovered window. Repeat with a floating window:
   hovering an exposed part should focus it without raising it over other windows.
2. Leave the mouse still and change selection with the keyboard. Move within the
   same window: keyboard selection should remain. Cross into another window to
   resume pointer-driven focus. Rofi and bars must not leak hover to windows below.
3. Drag a client selection or resize a window across another window. Focus must
   not jump during the held button or compositor interaction.
4. With two bordered tiles, use Mod+Control+F (`splith`) and Mod+Control+J (`splitv`).
   The selected tile's right edge should brighten for side-by-side placement; its
   bottom edge should brighten for stacked placement. Opening a terminal should
   follow that orientation. Floating windows have no split hint.
5. In a private config, reload `focus_follows_mouse no` and verify click/keyboard
   focus still works. Restore `yes`, and change the fourth `client.focused` color
   to confirm the split highlight updates. Borderless/fullscreen windows stay bare.

## Smart gaps and borders

1. Leave one tiled terminal on a workspace. With the example's smart settings,
   it should fill the bar-adjusted work area without a pixel border or outer gaps.
2. Open a second terminal: spacing and pixel borders should return. Close it:
   the first should expand again. Repeat with a nested split rather than only siblings.
3. Float the second window or open a parent-declared dialog. The sole tile should
   remain expanded; the floating window keeps its normal frame.
4. In a private config copy, set either smart option to `off` and reload. Check
   that gaps and borders can be controlled independently without changing the tree.
5. Try `border normal` on the solo tile: its titlebar remains while the border
   disappears. With a second tile, the requested border width returns.
   Cycle Mod+B through all styles too; automatic hiding must not erase the saved
   width. Border changes during fullscreen take effect when fullscreen exits.
6. Hoist one of two retained slots. Its placeholder should still count, preserving
   the workspace layout and the other tile's spacing.

## Tabbed and stacked layout validation

1. Open three terminals. Mod+W creates tabs; Mod+S creates stacked headers.
   Only the active branch should show content, and its title should be highlighted.
2. Click each header and type. Left/right navigate tabs; up/down navigate stacks.
   Moving the pointer over inactive header text must not send clicks to hidden content.
3. Mod+E restores the previous split, then alternates its axis. Existing split
   proportions should return. `splith`/`splitv` still prepare a nested split.
4. Create a split inside a tab. Switching away hides both descendants; switching
   back restores that branch's remembered selection. Move children with the usual
   Mod+Shift+arrows, including extraction at the group edge.
5. Close the active tab, then close down to one. Focus should recover locally,
   and the next terminal should join the remaining tab group.
6. Select the group with Mod+P and move it to another workspace. Return and check
   order, layout, focus, and retained hoist placeholders.
7. Check the outer frame: tabs/stacks and content share one border and shadow.
   Only the top row's outer corners round; content meets the headers without
   corner holes. Switching to split/floating restores independent frames.
   Split inside a tab, then create another tab/stack group inside it: interior
   gaps, independent borders, shadows and rounding stay suppressed. Shared
   dividers use the configured border width and styling. Nested headers remain
   selectable, and extracting the nested group restores its own frame.
8. Fullscreen the active tab and exit. Headers disappear and return, without
   revealing inactive tab contents. Check mixed-scale outputs separately.

The user confirmed tabs/stacks and interaction work, and accepted the nested
shared-frame styling with interior dividers after visual testing. Identifying
the focused inner pane remains less clear with a group-wide outer border;
improving that indication is deferred under
[next improvements](master-tiling.md#next-improvements). Mixed-scale output
validation remains outstanding.

## Tiled mouse resizing

1. Open two terminals side by side. Drag the shared border with the left button.
   Both windows should resize smoothly; their tree/order should remain unchanged.
2. Create a vertical split on one side. Drag the corner where the three windows
   meet. Horizontal motion should move the outer split, vertical motion the inner.
3. Mod+right-drag toward an interior shared edge should perform the same resize,
   including on a client-decorated window. Outer edges without a neighboring tile
   are no-ops. Mod+left-drag continues to move floating windows only.
4. Release outside the window. Subsequent pointer motion must not continue resizing.
   Releasing another mouse button while holding the initiating button must not end it.
5. During a resize, close a neighboring window, switch workspace, toggle floating,
   or enter fullscreen. The old drag must stop without moving an unrelated split.
6. Repeat floating titlebar/modifier drags and CSD-native resizing. Reload pointer
   bindings while holding a drag, then release; there must be no stuck capture.
   Try a tiled CSD titlebar drag too: unsupported move requests must release
   cleanly when the button is released.

Automated coverage uses the shared border-press/motion/release adapter plus native
tile-policy tests for ancestor corners, bounded shares, invalid deltas, tree-change
cancellation, and competing requests in one batch. Existing floating settlement
and captured-release regressions remain in the suite.

## Fullscreen validation

1. With several tiles, use Mod+A. The selected app should fill the output,
   including the bar area, with no Weld frame/watermark. Mod+A restores the tree.
2. Repeat with a moved/resized floating window several times. Its original
   position and content size should return without growing or shrinking.
3. Open menus and a declared dialog while fullscreen. They should remain above
   the owner and accept input. Unrelated newly launched windows must stay hidden.
4. In normal fullscreen, Mod+Space should show Rofi; Escape returns input to the
   application. In exclusive mode (Mod+Shift+A), ordinary overlays remain hidden.
   Mod+A, workspace switching, and compositor exit shortcuts must still work.
5. Switch away and back. The other workspace should have its normal bar/layout;
   the original should still be fullscreen. Close the fullscreen owner and check
   that bars, other windows and input recover.
6. Test application-native fullscreen (F11 where supported, or a game's setting)
   under both Wayland and XWayland. Native exit should restore the same layout.
7. With multiple outputs, fullscreen one output. Other outputs must remain usable;
   an unrelated window overlapping the claimed output should not cover its owner.
8. Rebuild both hoist peers, then enter/exit fullscreen on a receiver and verify
   client resizing and reclaim. The separate XR shell keeps spatial sizing.

Automated coverage includes repeatable geometry restoration, output resize,
workspace suspension/return, owner destruction, related-dialog focus,
cross-output projection suppression, layer keyboard routing, and initial client
requests before mapping. A real Wayland protocol fixture checks atomic size/state
and constraint restoration. The bounded X11 probe verified fullscreen properties
and visual entry/exit in `target/validation/fullscreen-smoke-caonz1j0`.
The pre-map fullscreen-property variant also passed in
`target/validation/fullscreen-smoke-jx3u1o9r`; pending policy transitions suppress
the intermediate admission configure. Floating/tiled mode changes resume after
leaving fullscreen.

## Final automated pass and limits

The smart-settings assembly passed the X11 family lifecycle capture in
`target/validation/floating-smoke-1lsgvunz`. The parent has a bare solo-tile frame;
the transient dialog remains centered and decorated. Waybar and Rofi also mapped,
captured and tore down successfully in `target/validation/layer-shell-kyv30vv9`.
Their captures show the panel work area and overlay launcher preserved.
The header-only smart-border capture in `target/validation/floating-smoke-5i7fibg6`
also preserves the normal header and rounded client bounds.

Reproducible core checks:

```sh
cargo test -p weld-window -p weld-float -p weld-tile -p weld-ssd -p weld-i3-quirks -p weldwm --lib --locked -j 8
cargo test -p weld-i3-quirks --test behavior --locked -j 8
cargo clippy --workspace --all-targets --features weld-core/test-support --locked -j 8 -- -D warnings
```

Protocol/hoist tests also passed, including the real Wayland fullscreen configure
fixture, authorized fullscreen forwarding, ordered encoded control events, and
the existing hoist lifecycle suite. The Godot Rust desktop receiver compiled
after the shared configure shape changed. No Android deployment or physical DRM
input validation was performed during this phase.

Use a private config copy for reload experiments, retaining a working exit
binding (`Mod+Shift+Escape` in the example). Test on an unused TTY before replacing
the main login session. Direct scanout, fixed-size/type-based dialog hints,
per-app matching rules, and the rest of the Sway command language remain separate
work. The existing Vulkan acquire-fence validation messages are not fixed here.
