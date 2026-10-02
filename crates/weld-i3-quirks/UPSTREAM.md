# i3 behavioral reference

The reference revision is i3 `903bcd518df32b0e055b17f5da3f988a0187fd3d`.
Tests run against Weld's actual window and tiling plugins without a display server.
Adapted scenarios carry the upstream BSD-3-Clause notice in `LICENSE-i3`.

i3 is the behavioral reference where it and Sway differ. For example, i3 inserts
after the remembered child when entering a perpendicular branch; moving right
into a vertical branch therefore places the incoming window below that child.
Sway's `container_move_to_container_from_direction` at revision
`1652c54b73f67df17b7b4ab0b0f7048204aa8104` instead inserts before the destination
leaf for right/down moves. The direction/axis matrix deliberately retains i3's
rule. Sway syntax support does not imply Sway movement semantics.

This first batch adapts directional no-ops and sibling wrapping from
`testcases/t/121-next-prev.t`, and nested close restoration plus unfocused
destruction from `129-focus-after-close.t`. Branch-memory and ancestor traversal
cases additionally exercise `src/con.c` (`con_focus`, `con_descend_focused`,
`con_next_focused`) and `src/tree.c` (`get_tree_next`). Weld-specific cases cover
retained vacant slots, ownership transfer, pre-output requests and ordered batches.
Additional regressions preserve inactive branch recency when its selected leaf
closes and the tiler promotes its surviving child.

The wider test corpus remains the target for subsequent behavior slices:

- Parent/child and sibling selection: `101-focus.t`, `307-focus-next-prev.t`.
- Wrapping interactions: `170-force_focus_wrapping.t`, `308-focus_wrapping.t`,
  `539-disable_focus_wrapping.t`.
- Structural movement: `124-move.t`, `274-move-branch-position.t`,
  `306-move-to-parent.t`, `309-crash-move-parent.t`, `516-move.t`, `524-move.t`.
- Layouts: `122-split.t`, `131-stacking-order.t`, `192-layout.t`,
  `292-regress-layout-toggle.t`, `297-scroll-tabbed.t`.
- Floating selection and transitions: `005-floating.t`, `135-floating-focus.t`,
  `146-floating-reinsert.t`, `236-floating-focus-raise.t`,
  `273-regress-focus-toggle.t`, `520-regress-focus-direction-floating.t`.
- Workspace/output, fullscreen, scratchpad and remaining regression coverage
  follow those capabilities. X11-specific mechanisms require platform-appropriate
  equivalents rather than importing the Perl/X11 harness.

This is partial scenario coverage, not a claim that these whole files or the
complete suite have been ported. Each later slice should record its adapted
cases and explicit platform exclusions here.

Source: https://github.com/i3/i3/tree/903bcd518df32b0e055b17f5da3f988a0187fd3d/testcases

## Structural movement coverage

Master routes directional movement through `I3MoveRequest`. The native geometric
swap remains available separately. `tests/behavior.rs` includes `cases/movement.rs`
and exercises the following adaptations at the pinned revision:

- `124-move.t`: solitary-window no-ops; adjacent-leaf reordering with no edge
  wrap; entering an adjacent split; reordering inside it; extracting back out;
  and removing a source split after its last child moves away. The floating
  sections remain deferred (steps, explicit pixels/percentages and positioning).
- `274-move-branch-position.t`: when movement matches the destination layout's
  axis, enter at the near edge (right/down prepend, left/up append). When it is
  perpendicular, insert after the destination's remembered child. Repeat from
  both a workspace leaf and a nested source. Weld runs all 16 direction/axis/source
  combinations with horizontal/vertical splits. The original tabbed/stacked
  variants remain deferred until those layouts exist.

  Both branch-entry sites in the pinned
  [move.c](https://github.com/i3/i3/blob/903bcd518df32b0e055b17f5da3f988a0187fd3d/src/move.c)
  use this position expression (whitespace condensed):

  ```c
  con_orientation(target->parent) != o || direction == D_UP || direction == D_LEFT
      ? AFTER : BEFORE
  ```

  In particular, a perpendicular destination selects `AFTER` for every direction.
- `306-move-to-parent.t`: preserve sibling position and selection when lifting
  a child into its grandparent, for both workspace and split destinations. The
  upstream command sequence uses marks and criteria; `weld-tile`'s structural
  tests exercise the same two transformations directly. The command sequence
  and selection of groups remain deferred.

Additional cases follow `src/move.c`, `con_descend_direction` and `tree_flatten`:
workspace reorientation, nested destination descent, alternating-wrapper cleanup,
batched moves, focus recovery immediately after reparent, and size-share handling.
A singleton in a direct workspace child remains grouped at the workspace edge
when no adjacent output exists, matching `tree_move`'s directed-output branch.
Weld-specific coverage protects retained slots, startup ordering, invalid edits
and depth-limit termination. Native flattening retains relative child proportions.

`516-move.t` and `524-move.t` were inspected but require multi-output/workspace
and fullscreen or stacked-layout support. Those suites remain deferred, along
with group selection, floating movement, marks and criteria. This batch does not
claim those command or layout surfaces.

## Workspace foundation coverage

`tests/cases/workspace.rs` adapts the following cases at the same pinned revision:

- `297-assign-workspace-to-output.t`: initial assigned names, binding names and
  unused numbers across four outputs; ordered connector fallbacks; exact-name
  precedence over numeric assignments; primary/nonprimary selection. Rename
  and directional output-move cases remain deferred.
- `503-workspace.t`: global numbered cycling, equal numeric prefixes on distinct
  outputs, and next/previous-on-output wrapping.
- `176-workspace-baf.t`: explicit back-and-forth recreates retired empty
  workspaces, and switching to the selected workspace is a no-op. Automatic
  back-and-forth, rename, restart and scratchpad variants remain deferred.

Policy also follows `src/workspace.c`: `workspace_get`, `get_assigned_output`,
`create_workspace_on_output`, `workspace_show`, and next/previous selectors.
First assignment directives win, and a missing assigned output falls back to
the current output. Creation uses binding order before unused numeric names.
Weld-specific coverage checks relationship inverses, per-tree geometry and
focus, cross-workspace transfer, startup batches, hidden-slot occupancy,
inactive close recovery, and output disappearance/reappearance.

This slice preserves existing workspace assignment on config reload; applying
new assignments to already-existing workspaces remains a separate policy change.
Directional output traversal, pointer warping, group selection and floating
workspace movement are not included.
