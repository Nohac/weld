# Weld Master: first native tiling slice

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
alternatives. Shift adds leaf swapping,
and Control adjusts width/height proportions. Alt+Shift+Q requests client close.
Alt+H invokes `weld hoist` through the existing hoist policy. Firefox is now
Alt+Control+Shift+F, leaving Alt+F for focus-right. Ordinary clicks select tiles;
decoration close buttons work. Pointer-driven floating movement is not installed.

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
  With the current single workspace, `workspace` behaves like `yes`.
- `bindsym CHORD COMMAND` with literal Mod4, Mod1, Control/Ctrl and Shift
  modifiers, lowercase ASCII letter names, arrow/F1-F12 keys, Return, Escape,
  equal and minus. Trigger names currently select physical key positions; modifiers follow
  Weld's configured XKB map. Full symbolic `bindsym` matching is a follow-up.
- `input type:keyboard { ... }` or `input * { ... }`, and their single-line
  forms, with `xkb_rules`, `xkb_model`, `xkb_layout`, `xkb_variant`, `xkb_options`.
  Values are literal XKB names, optionally quoted; empty quoted options clear
  configured swaps. Per-device selectors and variable expansion are follow-ups.
  Directives apply in source order to the single native seat.
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
i3 directional navigation and close-focus restoration over the shared tree.
Master supplies the interpreter's typed command extension for the `weld`
vocabulary and executes shell/distribution effects.
Applying a valid candidate
replaces its owner-scoped keyboard bindings and settings through a typed
`SystemParam` borrowing just those resources. Old queued binding IDs cannot invoke new bindings, and consumed
key releases remain consumed across replacement.

Gaps relayout existing windows. Default orientation does not rewrite existing
containers. Commands execute in order, resolving focused-target commands at
execution time. Master queues typed shortcut events; each observer's deferred
effects finish before the next shortcut is resolved. A focus action triggers
`I3FocusRequest`; structural actions trigger `TileRequest`, so focus followed by
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
single-child splits remain available for the next admission. Directional moves
currently swap leaf positions, not whole subtrees or Sway's full move semantics.
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
Parent/child group selection, tabs/stacks, `focus mode_toggle`, floating behavior
and structural i3 movement remain follow-ups; their commands are still rejected.

## Deliberate boundaries

This is one workspace on the primary output. Multiple workspaces, multi-output
placement, floating dialogs/overlays, tabbed/stacked layouts, fullscreen,
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
replacement/release behavior. These do not constitute a multi-output or complete
Sway compatibility test.

The initial nested smoke run completed with three real `foot` windows tiled
across the output and a captured screenshot. It also emitted Vulkan acquisition
fence-validation errors; this slice does not claim a warning-free graphics path.
