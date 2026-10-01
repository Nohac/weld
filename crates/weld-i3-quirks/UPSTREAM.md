# i3 behavioral reference

The reference revision is i3 `903bcd518df32b0e055b17f5da3f988a0187fd3d`.
Tests run against Weld's actual window and tiling plugins without a display server.
Adapted scenarios carry the upstream BSD-3-Clause notice in `LICENSE-i3`.

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

## Next: structural movement

Master still routes directional movement to the native leaf-swap operation.
The movement slice must replace that interpretation rather than changing the
meaning of the native swap for other consumers. The inspected upstream cases are:

- `124-move.t`: solitary-window no-ops; adjacent-leaf reordering with no edge
  wrap; entering an adjacent split; reordering inside it; extracting back out;
  and removing a source split after its last child moves away. Floating sections
  also cover default 10-pixel steps, explicit pixels/percentages and positioning.
- `274-move-branch-position.t`: when movement matches the destination layout's
  axis, enter at the near edge (right/down prepend, left/up append). When it is
  perpendicular, insert after the destination's remembered child. Repeat from
  both a workspace leaf and a nested source, for tabs and stacks.
- `306-move-to-parent.t`: preserve sibling position and selection when lifting
  a child into its grandparent, for both workspace and split destinations. The
  upstream command sequence uses marks and criteria; the structural tests can
  exercise the same tree transformation before those command features exist.

These are queued behavioral cases, not yet passing movement tests in Weld.
