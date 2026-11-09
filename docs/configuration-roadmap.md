# Configuration coverage and next steps

Status: **implemented capability audit plus proposed work for review**.
Reference: [Sway 1.12 configuration and command documentation](https://github.com/swaywm/sway/blob/1.12/sway/sway.5.scd).
The checked-in i3 grammar inventory is a provenance inventory, not a Sway
compatibility percentage. This audit groups gaps by behavior and owner; it does
not claim to enumerate every Sway command variant.

## Existing runtime capabilities exposed in this batch

| Configuration | Existing owner/path | Validation |
| --- | --- | --- |
| `bindcode [MODIFIERS+]CODE COMMAND`, also inside modes | `weld-input` physical shortcut matching and release ownership | XKB-to-evdev conversion, malformed codes, duplicate named/numeric identities, real raw-key dispatch in a synthetic app |
| Additional named keys and Alt/Super aliases | Generated physical key mapping | BackSpace/navigation/keypad/extended function keys and duplicate aliases |
| `focus tiling` and `focus floating` | `weld-i3-quirks` workspace focus history, shared with `focus mode_toggle` | Explicit selection is idempotent; an empty plane preserves focus |
| Bound `move absolute position X Y` | Distribution window action executor and `weld-float` move intent | Nonzero output origin; focused floating target; finite coordinate validation |
| `for_window … fullscreen [enable\|disable\|toggle]` | `weld-window::fullscreen` | Matching background occupant rather than current focus; once-per-match/reload behavior |
| `for_window … sticky enable\|disable\|toggle` | Existing i3 sticky policy | Explicit matched target and once-per-match/reload behavior |

The generated syntax, feature-owned contracts, native lowering, and runtime
publication boundaries remain in place. Numeric keycodes are XKB numbers
(evdev + 8); named `bindsym` keys still use Weld's documented physical-position
subset. The new grammar does not claim layout-aware keysym matching.
Fullscreen rules use the existing queued presentation transition. Sticky
windows follow workspace changes only when floating, on their current output.

## What remains, and why it is more than adding a spelling

| Family | Existing foundation | Missing behavior |
| --- | --- | --- |
| Source composition | Variables, quoted payloads, blocks, source-located diagnostics | Includes, path/glob expansion policy, multi-file source provenance |
| Commands and selection | Typed actions, metadata rules, sequential shortcut dispatch, read-only Sway IPC | General command sequences, runtime criteria, marks, broader targetable rule actions, IPC command execution |
| Shortcuts | Physical keyboard chords, modes, consumed-release tracking, pointer interaction chords | Keysym/group matching, release/repeat/device/locked/inhibited flags, general pointer/wheel bindings, gestures and switches |
| Input preferences | Global XKB keymap; tap, tap-button map, click method; cursor theme | Natural scroll/acceleration/left-handed controls, repeat rate/delay, per-device keyboard policy, cursor hiding timeout, named seats |
| Output configuration | Output inventories, scale matching/publication, host-selected output topology | Configured position/mode/transform/power/VRR and transactions across backend reconfiguration |
| Output navigation | Workspace assignment/membership and synthetic multi-output tests | Named/directional focus and moves, workspace moves to outputs, fallback and hot-unplug policy |
| Layout policy | Split/tabbed/stacked layouts, geometry and relative resizing | `workspace_layout`, automatic/toggle split policy, absolute resize, per-workspace/per-side gaps, more smart-gap/border forms, group floating/fullscreen |
| Window state | Stable managed windows, workspace membership, fullscreen, persistence | Scratchpad lifecycle, marks and mark-targeted commands, richer/dynamic criteria, global fullscreen across outputs |
| Startup services | CLI-selected XWayland and external bar/application launch | Config-owned startup-only settings and reload diagnostics; lazy XWayland; fuller bar/background helper lifecycle |
| Decorations | Borders, colors, font preferences, corner radius | Title formatting/alignment and other Sway decoration options |

Some foundations are close but are not complete policies. For example,
`WorkspaceRequest::Assign` can reassign membership, but a Sway workspace move
also needs destination selection, visibility reconciliation, focus, and empty
output handling. Likewise, having tabbed layouts does not implement a default
layout policy for newly created workspaces.

Weld's exclusive fullscreen suppresses ordinary overlays on one output; Sway's
`fullscreen global` spans outputs. They must remain distinct.

## Proposed implementation order

### 1. Configuration includes

Own file loading and include expansion in `weld-sway-config`, with an explicit
file-loading entry point alongside the pure in-memory parser. Keep filesystem
access out of native IR and runtime publication. Preserve declaring-file-relative
paths, variable ordering, diagnostic source identity, and whole-candidate
validation before publication or startup effects.

Resolve the supported path-expansion subset before coding. Sway documents shell
expansion; accepting command substitution would execute code during validation.
Specify path/glob/environment handling and reject unsupported expansion rather
than approximating it silently. Define repeated includes, cycles, depth/byte
limits, and missing matches explicitly.

Acceptance: nested relative includes, deterministic glob order, shared/redefined
variables, repeated/cyclic includes, correct file/line errors, and a failed
included file leaving live settings and startup queues untouched.

### 2. Shared command execution and target selection

Add generated typed command sequences and frontend criteria syntax. Keep
metadata/window selectors with `weld-window-config`; put cross-feature command
assembly in `weldwm-config` and execution in the distribution. Reuse targeted
feature requests for bindings, window rules, and future IPC command execution.
Broaden the currently limited rule-action vocabulary here, including toggled
borders/floating, resize, and other meaningful targeted commands.

Define when each selector resolves, ordering across deferred ECS effects, and
error results for each command. Preserve Sway's comma/semicolon criteria scopes
and opaque `exec` shell text. Closing or moving a target must not accidentally
retarget the next operation to an unrelated newly focused window.

Acceptance: quoted separators and shell commands; mixed sequences; explicit
background targets; target destruction; mode/focus changes within one input
batch; partial command errors; atomic configuration reload. Add Sway IPC command
execution only through this executor, with no independent parser or dispatcher.

### 3. Shortcut semantics and device preferences

Extend the input contracts and raw-input matcher together. Start with true
keysym/layout-group matching, then explicit trigger/repeat policy and general
pointer/wheel bindings. Input owns press/release capture across mode changes,
reloads, and focus changes. The config frontend only translates spelling.

Define priority for physical versus symbolic bindings and optional device
matches. Define locked/inhibited routing against actual session/input-inhibition
state before accepting those flags. Pointer bindings need hit regions and the
window under the pointer, rather than blindly using keyboard focus. Keep wheel
steps distinct from held buttons. Gestures and switches follow when their host
events and ownership are available.

Device preferences need new owned fields and host application, including
hotplug and native-default restoration. Repeat ownership already exists, but
configurable repeat rate/delay does not. Preserve the nested host's relationship
to parent-supplied cadence.

Acceptance: alternate layouts, keymap changes while held, mode/reload transitions,
repeat cancellation, device removal, lock/inhibit transitions, pointer regions,
wheel direction/steps, and unsupported hardware reporting. Use synthetic input
first, then bounded headless/nested protocol tests.

### 4. Output topology and navigation

Extend `weld-output` with validated desired settings; the host owns capability
checks and applying modes/placement/transform/power/VRR. `weld-app` publishes the
accepted topology. Desktop policy consumes that topology for named/directional
focus, moving windows/workspaces, output removal, and fallback destinations.

Establish candidate validation and failure handling before exposing broad output
syntax. Logical layout coordinates, physical placement, scale and transform must
have explicit conversions; mutating ECS output geometry alone is insufficient.

Acceptance: synthetic multi-output selection/visibility/geometry tests;
headless/nested capture and input-coordinate checks; bounded DRM validation for
physical modes, power and VRR. Headless evidence cannot establish hardware support.

### 5. Remaining layout and window policies

Implement concrete slices independently: default workspace layout and split
choices; absolute resize and richer gaps; marks and dynamic criteria; scratchpad
state; then group operations and spanning fullscreen if desired.

`weld-tile-config`/`weld-tile` own geometry and tree contracts, while the selected
i3 policy owns Sway navigation choices. Window identity, marks and retained state
belong with window management. Scratchpad is not just an invisible workspace:
define show/hide cycling, focus restoration, floating geometry, occupancy and
persistence, hoisted clients, and capture visibility.

Acceptance: adapted upstream behavior cases and synthetic lifecycle tests for
empty/populated workspaces, destroyed or replaced occupants, focus restoration,
and output changes. Add each syntax form only with its complete policy.

### 6. Startup settings and presentation extras

Separate startup-only host preferences from live desktop settings. XWayland
currently starts eagerly when enabled by CLI. Sway distinguishes lazy `enable`
from eager `force`, so supporting those spellings requires an explicit host
lifecycle decision. Define CLI precedence and a clear restart-required error for
reloads that change immutable settings; do not silently ignore changes.

Review bar/background helper ownership separately from launching arbitrary
commands. Add title formatting/alignment where the decoration runtime can consume
validated preferences. Avoid claiming compatibility with an external bar's own
configuration language just because Weld can launch it.

## Review gates

Each batch includes generated definitions/output, native translation, focused
behavior tests, and current coverage documentation. Use frontend dependency
checks and the desktop build graph; tests must not open windows on the user's
Weld session. Keep unsupported commands explicit until their runtime path works.

The next proposed implementation is includes, followed by shared command
execution. The later ordering is a recommendation for review, not authorization
to implement all remaining features in this batch.

Ownership review: the implemented batch passes all eight software-design skill
diagnostic checks (10/10); no failed rows. Existing input, focus, movement and
fullscreen implementations remain behind typed requests. The planned shared
executor addresses the remaining duplication pressure as command coverage grows.
