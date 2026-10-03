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

Further milestone checks will be added as implementation and validation land.

## Decoration validation

1. Tiled terminals should start with a narrow pixel border and no titlebar.
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
