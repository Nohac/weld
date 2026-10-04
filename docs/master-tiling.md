# Weld Master tiling and workspaces

The graphical `weldwm` distribution combines `weld-tile` workspace management
with `weld-float` freeform interaction for selected windows. Headless session hosting rejects
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
decoration close buttons work. Alt+Shift+Space toggles floating; Alt+Control+P
selects the most recently focused window in the other layout plane. Alt+left
drag moves floating windows, and Alt+right drag resizes from the pointer's quadrant.
Tiles resize at shared split boundaries, through a border drag or the same
modifier-right drag toward an interior edge.

Alt+P selects the parent split; repeated presses climb to the workspace root.
Alt+Control+Shift+P selects its remembered direct child. The selected group's
Weld borders use the focused palette while keyboard input stays with its
remembered client. Directional move, percentage resize, split preparation,
close and move-to-workspace commands target the selected group. Clicking a
window or switching window/workspace focus returns to leaf selection.
Splitting a selected group prepares a new enclosing split without changing the
group's internal layout; the next tiled window is admitted beside the group.
Alt+Shift+P toggles stickiness: a floating window stays on its output when that
output changes workspace. A tiled window can retain the preference, but it
takes effect only while floating. Disabling stickiness leaves it in the current
workspace. Retained vacant/hoisted slots obey the same policy.

The example starts Waybar with `examples/waybar.jsonc` and `examples/waybar.css`;
launch from the repository root so those relative paths resolve. Waybar's
`ext/workspaces` module displays existing workspaces and switches them on click.
Alt+Space opens Rofi's application launcher (`drun`). Both programs must be
installed, with native Wayland support in Rofi.

Alt+1..9 and Alt+0 select workspaces 1..10. Add Shift to move the selected
window without following it. These are logical Alt bindings; the example's
keymap makes physical Windows produce that modifier.

Graphical Master requires an explicit `--config` path. The repository's
`examples/master.sway.config` is a development example, selected explicitly by
the graphical hoist and profiling launchers. A supplied file may be a Sway
config; unsupported features are skipped with source-located warnings. Malformed
supported settings still reject the load. Master has no implicit config
discovery or bundled fallback. Headless session hosting needs no config.

Alt+Shift+R reloads the selected file. The file replaces, rather than overlays,
the configuration plugin's bindings; omitted settings return to their native
defaults. Parse/translation failure leaves the old bindings and settings active.
There is no file watcher yet; reload rereads the supplied path.

Check a file without opening a compositor, connecting a transport or running its
startup commands:

```sh
cargo run -- --validate-config --config ~/.config/sway/config
```

The check reports usable binding/startup-command counts and every skipped feature.
An `exec` command is preserved, not checked for availability or compatibility:
for example, `swaymsg exit` still addresses Sway, not Weld. Use a binding to Weld's
supported `exit` command when running a Sway-derived config. Waybar's Sway IPC
modules also need their Weld-supported equivalents; loading a bar command does
not provide Sway IPC compatibility.

## Supported configuration

- `set $name VALUE`, with ordered substitution in bindings, settings and commands.
  Values can refer to earlier definitions; redefinition affects later lines.
  Longer variable names match first. Expansion is single-pass, including inside
  quotes, and reparses the expanded spelling. Unknown shell variables such as
  `$HOME` remain for the shell; unresolved variables in required literal/chord
  values are errors. `$$` protects a dollar sign and backslash-escaped dollars
  remain escaped. Expanded text and the variable table are each limited to 8 MiB.
  The pure syntax parser remains unexpanded; the optional `evaluate` feature owns
  this pass. Prefix/redefinition behavior follows [Sway 1.12's variable replacement](https://github.com/swaywm/sway/blob/1.12/sway/config.c)
  and [set handler](https://github.com/swaywm/sway/blob/1.12/sway/commands/set.c).
- `gaps inner N` and `gaps outer N`, in logical pixels, integer 0..65535.
  Geometry clamps gaps to available space.
- `default_orientation horizontal|vertical`, for new workspace roots only.
  Split-edge highlighting appears only after an explicit split command, on the
  right for horizontal insertion or bottom for vertical insertion. It clears
  when the split gains another child; an ordinary layout direction is unmarked.
- `smart_gaps on|off` removes outer gaps around a sole tiled slot.
- `smart_borders on|off` hides that slot's border. Pixel frames become bare;
  normal frames retain their header. Floating windows keep their configured frame.
  `no_gaps`, `inverse_outer` and runtime smart-setting commands remain unsupported.
- `focus_wrapping no|yes|force|workspace` (default `yes`). Ordinary wrapping
  first searches ancestor splits for a directional neighbor, then uses the
  innermost wrap candidate. `force` wraps at the first eligible split edge.
  Directional output traversal is a follow-up, so `workspace` currently behaves like `yes`.
- `focus_follows_mouse yes|no` focuses a visible window when mouse motion enters
  it, without raising floating windows. The Sway interpreter enables it by default.
  Held buttons and active move/resize sessions suppress hover focus. Keyboard
  selection and layout changes beneath a stationary pointer retain focus until
  the pointer crosses into another window. `always` remains unsupported.
- `floating_modifier MODIFIER[+MODIFIER...]` configures shared window pointer chords:
  move for floating windows and resize for floating or tiled windows.
  An optional trailing `normal` is accepted; `inverse` remains unsupported.
  Reload replaces these bindings while retaining already captured releases.
  Omission disables modifier pointer chords; titlebar and resize handles remain usable.
- `floating enable|disable|toggle` and `focus mode_toggle` are bound commands.
- `default_border normal|pixel [N]`, `default_floating_border normal|pixel [N]`
  and `none`, with widths 0..64 logical pixels. `new_window` and `new_float`
  are aliases. `normal` has a titlebar; `pixel` retains only the border; `none`
  removes chrome, shadow and resize hit areas. Modifier-based floating controls
  remain available. An omitted width uses 3 pixels.
- `corner_radius N`, 0..64 logical pixels, applies to decorated frames.
- `client.focused`, `client.unfocused`, `client.focused_inactive` and
  `client.placeholder` accept three to five `#RRGGBB`/`#RRGGBBAA` colors: border,
  background, text, indicator and child border. Omitted indicator/child-border
  colors use the border and background colors respectively. Normal frames use the first
  color on the upper border and child-border color elsewhere; pixel frames
  use child-border color throughout. Titlebar fill and close glyph use background
  and text. The focused tile uses indicator color on its right edge for side-by-side
  layout, or bottom edge for stacked layout, marking where the next window opens.
  The hint follows the current parent split and
  adds no chrome when smart borders, fullscreen or an
  explicit borderless style hides that edge. An active move/resize uses indicator
  color on all edges. The inactive palette
  currently identifies the parent of the focused dialog. Relocated windows
  retain their distinct configured red accent. Urgency hints remain a follow-up.
- `border normal|pixel [N]|none|toggle` changes the selected window's frame.
  Per-window overrides survive global reload; live defaults apply to other windows.
- `bindsym CHORD COMMAND` with literal Mod4, Mod1, Control/Ctrl and Shift
  modifiers, lowercase ASCII letter names, digits, arrow/F1-F12 keys, Return, Escape, Tab, Pause, Print, space,
  equal and minus. Trigger names currently select physical key positions; modifiers follow
  Weld's configured XKB map. Full symbolic `bindsym` matching is a follow-up.
- `input type:keyboard { ... }` or `input * { ... }`, and their single-line
  forms, with `xkb_rules`, `xkb_model`, `xkb_layout`, `xkb_variant`, `xkb_options`.
  Values are literal XKB names, optionally quoted; empty quoted options clear
  configured swaps. Per-device selectors remain a follow-up.
  Directives apply in source order to the single native seat.
- `workspace NAME output CONNECTOR [FALLBACK...]`. The first available connector
  wins; `primary` and `nonprimary` are supported selectors. An exact name rule
  takes precedence over a digits-only number rule. The first directive for an
  exact workspace name wins. Missing connectors fall back to the current output.
- `workspace NAME`, `workspace number NAME`, `workspace next|prev`,
  `workspace next_on_output|prev_on_output`, `workspace back_and_forth|current`.
  The `number` form finds a leading number even in a name such as `3: work`.
  Quoted reserved words remain literal names. Names can contain spaces; escapes,
  unresolved variables and command sequences are rejected in this subset.
- `move [container|window] to workspace TARGET` and `move workspace TARGET` use
  the same targets and move an individual selected window without following it.
- `exec [--no-startup-id] COMMAND` starts a command once after initial configuration.
  `exec_always` also starts it after each successful reload. Commands keep their
  source order and use the host-owned client launcher, with Weld's Wayland socket
  and toolkit environment. Failed configuration reloads launch nothing.
  `--no-startup-id` is accepted; activation tokens are not currently issued.
- `bar { swaybar_command COMMAND }` starts an explicitly supplied bar command
  once, using the same startup queue as `exec`. Other bar fields warn and skip;
  the bar application owns its configuration. No default `swaybar` is started.
- Bound commands: `splith`, `splitv`, `split h|v|horizontal|vertical`,
  `focus left|right|up|down`, `move left|right|up|down`,
  `resize grow|shrink width|height N ppt`, `kill`, `reload`, `exit`, and `exec COMMAND`.
  Exec runs the preserved shell command through `sh -c` when invoked, never
  during parsing. A whole-command pair of outer quotes is removed when it is
  the sole argument; quotes within a multi-argument command are retained for
  the shell. Bound `exec_always` behaves like bound `exec`.
  Bindings fire once per fresh press, not
  repeatedly while held.
- Weld-specific commands use the existing command grammar: `weld hoist`,
  `weld output-debug`, and `weld scale increase|decrease|physical`. Scale bindings
  are registered only on DRM. Persistent-window matching is not yet exposed.

- `for_window [CRITERIA] border normal|pixel [N]|none` applies a per-window frame
  choice. Criteria support `all` and regex matches for `app_id`, `class`,
  `instance`, and `title`, combined with AND. Missing native/X11 properties match
  as empty strings, following [Sway's criteria matching](https://github.com/swaywm/sway/blob/1.12/sway/criteria.c).
  Thus `[class="^.*"]` also matches native Wayland windows. Rules run in source
  order, once per matching occupant, including when a later title first matches.
  Later title changes preserve manual border toggles. Successful reload retries
  the new rules against existing windows; removing a rule leaves its previously
  applied border choice intact. Matching uses Rust `regex`; PCRE-only constructs
  such as look-around/backreferences and dynamic `__focused__` matching are not
  supported. X11 class remains the neutral application label for hoisting, while
  criteria distinguish class/instance from native Wayland `app_id`.

Includes, general criteria commands, command sequences, modes, and other blocks remain follow-ups.
Unsupported mode blocks are skipped whole, so their bindings never become global.
`include` remains an error to avoid silently dropping a file's essential bindings.
Unknown input devices, unsupported keys, SwayFX effects and other window-rule actions warn and
skip. Volume, mute, play/pause, next and previous media-key names are supported;
brightness and the separate XF86AudioPause key remain unsupported.
The pure parser preserves more syntax than
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
test to exercise a changed assignment. Master publishes workspace names, active
states and output membership through `ext-workspace-v1`, including click-to-switch
activation. See [desktop layer surfaces](layer-shell.md#workspace-controls).
Sway IPC, pointer warping, blank-output pointer focus, `focus output`, `move workspace
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
Tabs/stacks, floating an entire selected group and group fullscreen remain
follow-ups; floating/fullscreen commands currently target the keyboard-focused
window. Directional cross-output movement remains a follow-up. Native
`TileOperation::Move` remains the geometric
leaf swap as a native primitive; Master's Sway `move` bindings use the i3
policy instead. Resized shares follow same-parent reordering, cross-parent arrivals
receive the mean existing share, and flattening preserves internal proportions.

## Deliberate boundaries

Tabbed/stacked layouts, resize-increment/aspect hints, persistent matching and
IPC remain follow-up slices. Popups keep the existing presentation path rather
than becoming tiling leaves. Declared transient toplevels float and center over
their parent's geometry. Unparented dialog, splash, utility and toolbar windows,
and windows fixed on either axis, float and center in the active workspace's
output work area. These decisions use protocol hints, never application names.
Popups retain their own
protocol positioning and never become floating managed windows.

### Mixed floating and tiled windows

The workspace retains `ManagedBy`, membership, visibility and output ownership
for both modes. `FloatingWindow` selects the freeform plane. Switching to floating
removes the leaf from layout and retires empty split containers; switching back
restores its former slot when that parent still exists, otherwise appends to the
workspace root. The last freeform geometry survives a tiled interval. Occupancy,
window identity and hoist state are preserved.

`FloatBehaviorPlugin` supplies movement, resizing and stacking for those marked
windows. `FloatPlugin` remains the separate floating-first admission assembly.
Workspaces remember floating selection and support moving floating windows to
another workspace without adding a tile. Native Wayland and X11 size hints and
X11 window types travel with the toplevel role, including through hoisting.
Floating geometry and configure requests respect minimum/maximum client sizes;
tiled and fullscreen configures use the assigned area. Wayland clients receive
tiled-edge state, and X11 clients receive the maximized state used for tiling.
The layout mode travels atomically with size/resizing/fullscreen, including on
remote reclaim. Rebuild both hoist peers after this wire-shape change.
Cross-output workspace transfers preserve output-local freeform geometry;
clamping a large floating window onto a smaller destination remains a follow-up.

### Mouse resizing

`WindowPointerPlugin` owns picking, modifier chords, native-grab requests,
frame-paced motion and matching-button release. Configuration publishes
`WindowPointerSettings`; float and tile policies accept supported interactions.
Acceptance installs the session, input owner and policy anchor atomically.

The tiler resolves each requested edge to a neighboring pair of split branches,
walking ancestors when necessary. Corner drags can adjust different ancestors
on the two axes. Motion changes only the pair's weights, preserving their total
share and a five-percent minimum per branch. Outer output edges without an
adjacent branch are no-ops. Tree changes, hidden workspaces and fullscreen retire
the active resize; a release of another mouse button leaves it active.

### Decorations

Master publishes `SsdSettings` alongside the other typed settings. Geometry
changes replace affected SSD projections before presentation metrics are
reconciled, preserving floating client-content size and allowing tiled layout
to keep its outer rectangle. Color changes update existing roots. Popup mounts
follow the replacement root through the shared window presentation contract.
An explicit per-window border rule or border command requests compositor-owned
presentation even when the client declares its own decorations. The same frame
decision is used by both presentation paths. Existing controls drawn inside the
application's content remain intact. Global default styles select metrics for
Weld frames, including frames around opaque remote content.
Mode classification happens before presentation claim so a newly admitted dialog
gets its floating frame metrics before centering. Per-window style belongs to
the durable window and intentionally survives vacancy and occupant replacement.

The example uses pixel borders for tiles and normal frames for floating windows.
Alt+B cycles the selected frame; Blender launch moved to Alt+Shift+B.
It enables smart gaps/borders, so a sole tile has no pixel frame or outer spacing.
The tiler publishes `SoleTiledWindow` from retained workspace slots; SSD consumes
that presentation fact without traversing the split tree. Floating windows are
excluded and retained hoist placeholders count as slots. Settings and the
per-window requested border style survive cardinality changes.
Border toggles operate on that requested style, including while fullscreen;
automatic hiding changes presentation without replacing the preference.

The basic on/off policy follows the [Sway command documentation](https://github.com/swaywm/sway/blob/master/sway/sway.5.scd).
Normal titlebar retention follows [Sway's view geometry](https://github.com/swaywm/sway/blob/master/sway/tree/view.c).

## Fullscreen presentation

`fullscreen enable|disable|toggle` translates to the native `FullscreenPlugin`.
The example uses Mod+A; it also exits exclusive fullscreen. Mod+Shift+A invokes
the Weld extension `weld fullscreen exclusive`, toggling the exclusive variant.

Both variants retain workspace/tree membership, suppress unrelated windows,
remove Weld chrome, and configure the client to the output logical size. Normal
fullscreen keeps overlay launchers available. Exclusive fullscreen suppresses
ordinary layer-shell presentation and keyboard routes, including overlays;
compositor shortcuts remain active. Related declared dialogs and popups remain
usable. The ordinary frame, freeform geometry and stacking return on exit; tiles
use the current layout if output size or other windows changed in the meantime.
Switching workspaces suspends the output claim until the workspace returns.
Exit fullscreen before changing between floating and tiled layout.

Wayland and X11 client requests share this policy. Requests survive admission;
a visible background client cannot steal fullscreen focus. Configure size,
resizing and fullscreen state travel together through the shared adapter and
hoist transport. Rebuild both peers after this wire-shape change. The XR shell
continues to own spatial window sizing and ignores desktop fullscreen intent.

Exclusive fullscreen here is presentation/input policy. Direct scanout and
suspending unrelated encoded streams are separate work. Physical multi-output
and DRM validation remain on the daily-driver checklist.

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
