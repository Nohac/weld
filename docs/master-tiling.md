# Weld Master tiling and workspaces

The graphical `weldwm` distribution now installs `weld-tile` instead of
`weld-float`. Both remain reusable policies; Master does not keep a legacy
floating-first assembly. Headless session hosting is unchanged and rejects
`--config`, since it does not install a graphical window manager.

## Try it

```sh
cargo run -- --backend nested --config examples/master.sway.config -- foot
```

Alt+Enter opens additional terminals. Alt+Ctrl+J prepares a vertical split
around the focused tile; open another terminal to populate it. Alt+Ctrl+F does the
same horizontally. Alt+D/F/K/J focuses left/right/up/down, with arrow-key
alternatives. Shift moves the selected window through the split tree,
and Control adjusts width/height proportions. Alt+Shift+Q requests client close.
Alt+H invokes `weld hoist` through the existing hoist policy. Firefox is now
Alt+Shift+Enter, leaving Alt+F for focus-right. Ordinary clicks select tiles;
decoration close buttons work. Pointer-driven floating movement is not installed.

Alt+1..9 and Alt+0 select workspaces 1..10. Add Shift to move the selected
window without following it. These are logical Alt bindings; the example's
keymap makes physical Windows produce that modifier.

Graphical Master requires an explicit `--config` path. The repository's
`examples/master.sway.config` is a development example, selected explicitly by
the graphical hoist and profiling launchers. A supplied file may be a Sway
config, but unsupported directives fail clearly. Master has no implicit config
discovery or bundled fallback. Headless session hosting needs no config.

Alt+Shift+R reloads the selected file. The file replaces, rather than overlays,
the configuration plugin's bindings; omitted settings return to their native
defaults. Parse/translation failure leaves the old bindings and settings active.
There is no file watcher yet; reload rereads the supplied path.

## Supported configuration

- `gaps inner N` and `gaps outer N`, in logical pixels, integer 0..65535.
  Geometry clamps gaps to available space.
- `default_orientation horizontal|vertical`, for new workspace roots only.
- `focus_wrapping no|yes|force|workspace` (default `yes`). Ordinary wrapping
  first searches ancestor splits for a directional neighbor, then uses the
  innermost wrap candidate. `force` wraps at the first eligible split edge.
  Directional output traversal is a follow-up, so `workspace` currently behaves like `yes`.
- `bindsym CHORD COMMAND` with literal Mod4, Mod1, Control/Ctrl and Shift
  modifiers, lowercase ASCII letter names, digits, arrow/F1-F12 keys, Return, Escape,
  equal and minus. Trigger names currently select physical key positions; modifiers follow
  Weld's configured XKB map. Full symbolic `bindsym` matching is a follow-up.
- `input type:keyboard { ... }` or `input * { ... }`, and their single-line
  forms, with `xkb_rules`, `xkb_model`, `xkb_layout`, `xkb_variant`, `xkb_options`.
  Values are literal XKB names, optionally quoted; empty quoted options clear
  configured swaps. Per-device selectors and variable expansion are follow-ups.
  Directives apply in source order to the single native seat.
- `workspace NAME output CONNECTOR [FALLBACK...]`. The first available connector
  wins; `primary` and `nonprimary` are supported selectors. An exact name rule
  takes precedence over a digits-only number rule. The first directive for an
  exact workspace name wins. Missing connectors fall back to the current output.
- `workspace NAME`, `workspace number NAME`, `workspace next|prev`,
  `workspace next_on_output|prev_on_output`, `workspace back_and_forth|current`.
  The `number` form finds a leading number even in a name such as `3: work`.
  Quoted reserved words remain literal names. Names can contain spaces; escapes,
  variable expansion and command sequences are rejected in this subset.
- `move [container|window] to workspace TARGET` and `move workspace TARGET` use
  the same targets and move an individual selected window without following it.
- Bound commands: `splith`, `splitv`, `split h|v|horizontal|vertical`,
  `focus left|right|up|down`, `move left|right|up|down`,
  `resize grow|shrink width|height N ppt`, `kill`, `reload`, `exit`, and `exec COMMAND`.
  Exec runs the preserved shell command through `sh -c` only when invoked, with
  the ordinary host-owned client launch environment, never during parsing.
  Sway-specific exec flags such as `--no-startup-id` are not interpreted yet;
  provide the shell command directly. Bindings fire once per fresh press, not
  repeatedly while held.
- Weld-specific commands use the existing command grammar: `weld hoist`,
  `weld output-debug`, and `weld scale increase|decrease|physical`. Scale bindings
  are registered only on DRM. Persistent-window matching is not yet exposed.

Variables, includes, criteria, command sequences, modes, and blocks other than
keyboard input configuration remain follow-ups. The pure parser preserves more syntax than
the distribution currently interprets. Master configuration owns launcher, exit,
output diagnostics and hoist bindings as well as tiling bindings. Reusable
plugins expose typed actions without installing those default keys. A supplied
empty config therefore installs no Master bindings. Reserved DRM Ctrl+Alt+Fn
console switching remains a separate backend escape mechanism.

## Keyboard mapping

The example config swaps Caps/Ctrl and left Alt/Windows, and binds Master
commands to logical Alt (Mod1). With those swaps, use the physical Windows key
for the shortcuts above; logical Super remains available to the parent
compositor. Change Mod1 to Mod4 in the config to choose logical Super instead.
These are explicit file settings in both nested and DRM modes.
The native fallback is US with no XKB options; `XKB_DEFAULT_*` environment
variables no longer supply keyboard configuration. Put those preferences here.

```sway
input type:keyboard {
    xkb_layout us
    xkb_options ctrl:swapcaps,altwin:swap_lalt_lwin
}
```

Weld resolves physical input using its own keymap before matching shortcuts,
then forwards unchanged physical keycodes to applications with that same
compiled keymap. The parent compositor can still consume shortcuts before Weld
receives them. Hold modifiers after entering the nested window; inheriting keys
already held on host focus entry is not implemented.

Keymap compilation happens before a candidate config is installed. Native
publication occurs between input batches and waits for local and client-visible
held keys to be released. A changed map starts with fresh lock/layout state;
reloading an identical map leaves that state intact. Legacy repeat settings
retain their existing ownership and are independent of XKB options.

## Ownership and live behavior

### Workspaces

`weld-window::workspace` owns session-stable workspace IDs, names, selection
history and local visibility. `WorkspaceMember`/`WorkspaceWindows` and
`WorkspaceOutput`/`OutputWorkspaces` are Bevy relationships with maintained
inverse collections. Removing an output clears its association without
destroying workspaces or managed windows. A `TileWorkspace` marker attaches a
tiling tree to that shared workspace entity; the tree has no global root.

The i3 plugin creates one visible workspace on each output. Initial names come
from assignments to that output, then switch bindings in file order, then the
first unused positive number. Existing workspaces stay on their assigned
outputs. Selecting one on another monitor changes management focus there;
the old monitor keeps its visible workspace. Creating an unassigned workspace
uses the currently focused workspace's output.

Switching preserves each tree, proportions and branch history. Moving a window
updates its layout edges, workspace relationship, output and visibility before
the next action. The source restores branch-local selection; the destination
remembers the arriving window. Hidden empty workspaces are retired and explicit
back-and-forth can recreate them by name. Retained vacancies and hoist slots
count as members and prevent retirement.

Visibility changes hide local window and popup presentations while retaining
client occupancy and hoist overrides. Consumer-driven frame activity/capture
integration is a later slice; this does not add a new suspension scheduler.
Output loss reassigns surviving workspaces to an available output. With no
outputs, windows remain hidden and retain their layout until one returns.
Production DRM connector hotplug itself remains outside this slice.

Reload publishes new creation/assignment preferences without moving existing
workspaces or rewriting their layouts. Use a fresh workspace or restart the
test to exercise a changed assignment. There is no workspace bar/IPC yet,
pointer warping, blank-output pointer focus, `focus output`, `move workspace
to output`, workspace rename, or automatic back-and-forth setting.

Example:

```sway
workspace 1 output eDP-1
workspace "2: recording" output DP-1 HDMI-A-1
bindsym Mod1+2 workspace number 2
bindsym Mod1+Shift+2 move container to workspace number 2
```

These output selection rules belong to `weld-i3-quirks`. The shared primitives
allow presentation policies to choose different workspace/output arrangements.

### Tiling and configuration

`weld-window` still owns managed-window identity, occupancy, focus effects and
client resize requests. `weld-tile` owns an ECS tree of ordered split containers
and managed-window leaves. Public queries expose container IDs, axes, weighted
children and parent edges. Entity references are process-local; stable window
and container IDs are separate. Persistence storage is not implemented.

The Sway backend's optional input module translates keyboard directives and
binding chords into `weld-input` types. Winnow captures chord tokens and quoted
values; the translation layer validates modifier names and aliases. Master
selects the file and installs typed settings. `weld-i3-quirks` consumes the Sway
parser/input output and translates supported settings and actions. It owns
i3 directional navigation, structural movement and close-focus restoration over
the shared tree.
Master supplies the interpreter's typed command extension for the `weld`
vocabulary and executes shell/distribution effects.
Applying a valid candidate
replaces its owner-scoped keyboard bindings and settings through a typed
`SystemParam` borrowing just those resources. Old queued binding IDs cannot invoke new bindings, and consumed
key releases remain consumed across replacement.

Gaps relayout existing windows. Default orientation does not rewrite existing
containers. Commands execute in order, resolving focused-target commands at
execution time. Master queues typed shortcut events; each observer's deferred
effects finish before the next shortcut is resolved. Focus and move actions trigger
`I3FocusRequest` and `I3MoveRequest`; other tiling actions trigger `TileRequest`, so focus followed by
hoist selects the updated target, while a
reload invalidates remaining old binding IDs in the same batch.

`TileSystems` orders focus recovery, workspace preparation, buffered commands, distribution
actions and final layout within window management. Admission and output changes
remain tiler-owned. Systems and observers declare their component/resource access
with queries and typed parameters; deferred spawns and reparents are published
before dependent layout or actions. Layout skips unchanged frames, traverses
borrowed child lists and writes geometry only when it differs. Membership changes
trigger pruning; retained client-buffer updates leave the tree untouched. The operation
queue is bounded to 256 and split depth to 64. Animation is not part of layout.
Native and i3 focus requests waiting for the first workspace share that queue,
so startup buffering preserves their relative arrival order.

Retained vacancies occupy real slots without fake client surfaces. The hoist
adapter can detach/reclaim occupancy and override presentation without changing
the leaf identity or slot. Ordinary window removal prunes the tree; explicit
single-child splits remain available for the next admission. Master directional
moves reorder adjacent leaves, enter neighboring branches, or extract the selected
window into an ancestor. Branch entry uses the near edge on a matching axis and
the remembered child on a perpendicular axis, recursively. A perpendicular move
at workspace level groups the remaining contents under their original axis and
reorients the outer workspace. Moving out preserves explicit one-child splits;
empty source groups are removed, and redundant alternating wrappers are flattened.
Movement retains selection and does not wrap at a workspace edge.
A singleton inside a direct workspace child also stays in its group at that
edge when there is no adjacent output to move onto.
Resizing transfers a share within the nearest matching-axis ancestor's adjacent
sibling pair, bounded to 5..95 percent of that pair; this is native policy, not
an exact Sway resize compatibility claim.

Directional focus in Master follows split ancestry rather than window-center
distance. Entering a sibling branch restores its most recently focused window;
closing the selected window first looks within its former branch, then climbs
outward. All accepted window focus changes contribute to shared tree-node history, including
clicks and new-window admission. Collapsed branches pass their focus-order position
to their surviving child. Retained slots participate even without an
occupant. Other users of `weld-tile` retain its default geometric navigation and
fallback policy unless they install and use the i3 actions.

The first adapted i3 scenarios and deferred test families are recorded in
[`weld-i3-quirks/UPSTREAM.md`](../crates/weld-i3-quirks/UPSTREAM.md).
Parent/child group selection, moving a selected group through i3 commands,
tabs/stacks, `focus mode_toggle`, floating and cross-output movement remain
follow-ups. The native editor can reparent a whole subtree, but Master currently
selects individual windows. Native `TileOperation::Move` remains the geometric
leaf swap as a native primitive; Master's Sway `move` bindings use the i3
policy instead. Resized shares follow same-parent reordering, cross-parent arrivals
receive the mean existing share, and flattening preserves internal proportions.

## Deliberate boundaries

Floating dialogs/overlays, tabbed/stacked layouts, fullscreen,
client-size constraint policy, complete tiled-state protocol hints, persistent
matching and IPC remain follow-up slices. Popups keep the existing presentation
path rather than becoming tiling leaves. Related toplevel dialogs currently tile
as ordinary windows.

## Next improvements

- **Host keymap inheritance.** Explicit Weld keyboard configuration works in
  nested and DRM hosts. Automatically inheriting a parent compositor's keymap
  remains a separate option; Winit currently hides that protocol information.
- **Directional native splits.** Express native split requests as Left, Right,
  Up and Down, including which side receives the next window. Map Sway/i3's
  horizontal/vertical commands through the compatibility adapter. Define native
  settings and operations around that directional capability; an internal axis
  may still serve the geometry calculation.

These are follow-up changes to the current implementation. Existing comment
wording will be improved when the relevant code is revisited, following the
contributor guidance on concrete, positive descriptions.

Automated checks cover split geometry, removal, vacant slots, occupancy
detach/reclaim, shared decoration-aware resize effects, live settings,
directional operations, batched navigation, config rejection, and shortcut
replacement/release behavior. Workspace integration tests use synthetic outputs
and adapted i3 scenarios; they do not establish physical multi-monitor validation
or complete Sway compatibility.

The initial nested smoke run completed with three real `foot` windows tiled
across the output and a captured screenshot. It also emitted Vulkan acquisition
fence-validation errors; this slice does not claim a warning-free graphics path.
