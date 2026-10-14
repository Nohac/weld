# Native window control

`weldctl` exposes the running compositor's managed-window inventory through CLI,
native JSON-RPC dispatch and stdio MCP. Windows include occupied applications and
vacant persistent slots. The standard nested and DRM assemblies publish the same
domain-owned inventory used by Sway IPC.

## Commands

Build Weld and the control executable in the usual development shell, then
restart Weld:

```sh
cargo build --locked -p weldwm -p weld-control --bins
weldctl --session weld-0 window list
weldctl --session weld-0 window list --json
weldctl --session weld-0 window get 0 --json
weldctl --session weld-0 window wait --after 1 --timeout-ms 5000 --json
```

Use an ID/revision from your own listing. Window, workspace and output IDs are
opaque strings belonging to the running host. `desktop_slot` uses logical compositor
coordinates and includes decoration. `application_viewport` reports the controlled
app's independent logical dimensions, capture pixel dimensions and `pixel_scale`.
Input uses `window_local_logical` coordinates; divide a capture pixel position by
the corresponding pixel scale to obtain an input position. The viewport becomes
available after the first controlled frame arrives.

Listings include application labels, occupancy, focus, visibility and floating
state. `control` is a random eight-character ID for the current application
incarnation, such as `k7Qm2xNp`. IDs are collision-checked and never reused within
the running session. `control_key`, such as `w12`, is a slot shorthand that resolves
to its current control ID. `capabilities` lists enabled permissions.

`window resolve w12 --json` resolves a copied shorthand. Operations accept either
form in their `control` argument. An incarnation ID fails after replacement;
using a shorthand deliberately follows the slot's current authorized occupant.
Empty persistent slots retain their shorthand and report that they are waiting
for an application. Resolution occurs once per request; queued operations remain
bound to the resolved incarnation and are cancelled when it retires.

## Remote control and live peek

Enable control from the desktop using this Sway-config binding:

```text
bindsym $mod+Control+i weld control toggle
bindsym $mod+Control weld control interact hold
bindsym $mod+Control+o weld control interact toggle
```

The repository example includes it. Alternatively, select the managed window
from `window list` and grant control through the local administrative CLI:

```sh
weldctl --session weld-0 window control --id WINDOW_ID --enabled
weldctl --session weld-0 window list --json
```

Controlled windows keep a header with View, Keyboard, Pointer, Resize, Interact,
Fit, Copy ID and Take back controls. Copy ID copies the stable shorthand for a
persistent slot and the incarnation ID for an ordinary window. It remains
available on vacant persistent slots. Clipboard delivery uses `wl-copy` against
the running Weld session; install `wl-clipboard` for this button.
Enabled permissions have an underline; disabled icons are
dimmed. Narrow frames show icons, and wider frames also show labels. The header
uses the configured title font and normal or persistent-window palette.

The local CLI can replace the enabled permissions (omitted flags disable them):

```sh
weldctl --session weld-0 window permissions --id WINDOW_ID --view --pointer
```

Changing permissions retires the previous handle and its held input, then issues
a fresh handle. The slot shorthand resolves to the new handle. Controllers using
an incarnation ID reconcile the inventory before continuing; requests
using the retired handle fail. The desktop peek remains visible when View is
disabled; that permission controls remote pixel capture and accessibility inspection.

Use the resulting `control` handle for operations:

```sh
weldctl --session weld-0 window resize --control CONTROL --width 1000 --height 700
weldctl --session weld-0 window scale --control CONTROL --factor 1.5
weldctl --session weld-0 window click --control CONTROL --x 100 --y 100 --button 272
weldctl --session weld-0 window press-key --control CONTROL --keycode 30
weldctl --session weld-0 window chord --control CONTROL --keycodes 29 --keycodes 30
weldctl --session weld-0 window scroll --control CONTROL --x 100 --y 100 --horizontal 0 --vertical 1 --wheel
weldctl --session weld-0 window capture CONTROL --json
weldctl --session weld-0 window control --id WINDOW_ID
```

The final command revokes control. `weldctl access revoke all` and the existing
`weld access revoke all` binding also revoke remote-control grants. After global revocation,
`weldctl access allow` explicitly enables new grants again.

One independent remote-control seat serves the granted windows. Operations preserve
the desktop's selected window. Keyboard codes are Linux evdev codes interpreted
through the configured host keymap. Pointer positions use window-local logical
coordinates; button codes are 272/273/274 for left/right/middle. Individual
`button` and `key` operations require matching release edges. Revocation releases
held input through the shared hoist relay.

`press-key` and `chord` validate the complete operation before dispatch. Chords
press in the supplied order and release in reverse order within one native batch.
They reject duplicate keys and keys already held through the low-level API.
Scroll positions are logical. With `--wheel`, deltas are wheel notches, including
fractional notches at v120 precision; otherwise they are logical pixels in one
completed finger gesture. Positive deltas scroll right/down. `scale` uses the shared
preferred-scale request, rounded to 1/120 units, under the Resize permission.
Omitting `--factor` restores the output's scale. Logical viewport size stays fixed;
capture metadata reports the application's actual raster density.

`type-text` types a complete Unicode string in one request through the controlled
keyboard seat:

```sh
weldctl --session weld-0 window type-text --control w6 --text 'Hello æøå 世界 🦀' --json
```

The equivalent MCP tool is `window_type_text` with `control` and `text` arguments.
It requires Keyboard permission and remote input ownership. Each request accepts
1–4096 UTF-8 bytes; longer input can be split at character boundaries. Explicit
newlines press Enter and tabs press Tab. Other control characters are rejected,
and Enter is added only when requested. Release low-level remote key holds first.
The result is an ordinary input receipt usable with `capture-after`.

Native Wayland delivery uses Smithay's temporary Unicode keysym mappings in
batches of at most 247 characters, restoring the seat keymap and modifier state
after each batch. Clipboard selections remain untouched. Tests verify exact
received UTF-8 bytes in Foot and a GTK entry, including multi-batch strings.

XWayland text requests currently return an explicit unsupported error before
focus or input effects. In the GTK/X11 probe, rapid temporary keymap changes
were interpreted using the wrong mapping and could trigger shortcuts, including
closing the dialog. X11 requires a separate text delivery backend. Existing
key/chord operations remain available for X11.

### Input sequences

`window_input` combines text and complete named keypresses/chords in one call:

```json
{
  "control": "w6",
  "actions": ["some text", {"key": "Return"}, "more text", {"key": "BackSpace"}, {"chord": ["Control", "a"]}],
  "typing_interval_ms": 25
}
```

CLI equivalent:

```sh
weldctl --session weld-0 window input --params-json '{"control":"w6","actions":["hello",{"key":"Return"}],"typing_interval_ms":25}' --json
```

All actions are validated before execution, including native key-name resolution
and text-backend eligibility. Named keys use XKB names, with aliases `Control`/
`Ctrl`, `Shift`, `Alt`, `Super`, `Enter` and `Backspace`. Keys resolve through the
controller's configured layout at admission, so Caps/Ctrl remapping is respected.
Chord modifiers are explicit; use text strings for exact character casing.
Key-only sequences also work with XWayland.

The interval defaults to zero and applies between Unicode scalar values within
each text action. It accepts 0–1000 ms, up to 64 actions and 4096 combined UTF-8
text bytes, with at most 25 seconds of planned typing. The host schedules each
step through native deadlines and gives the whole request a 30-second deadline.
Each chord releases its keys before another step or delay. A delayed host resumes
from the current time, preserving spacing instead of sending overdue characters
in a burst. `type-text` uses this same executor at zero interval.

The response reports `complete`, `completed_actions`, `typed_characters`, the last
`receipt`, and an optional `error`. Counts describe the prefix acknowledged by the
relay. `in_flight` marks a dispatched step whose completion is uncertain; inspect
the app before retrying interrupted work. Revocation, local takeover, replacement
or expiry stops remaining actions. The request retains its original occupant
even when addressed through a persistent-slot shorthand.

One sequence owns the remote controller at a time. Other remote mutations return
a busy error while it runs; inventory and capture remain available. Application
reactions are asynchronous: split the sequence and inspect/wait when a step opens
a dialog or otherwise changes the intended input target. A successful final
receipt can be used with `capture-after`.

### Local interaction

The desktop frame initially remains a live, read-only peek. Hold the configured
modifier chord to use the focused window locally, or use the toggle binding to
keep local keyboard and pointer input enabled after releasing the chord. The
header shows **Local interaction** while takeover is active. The **Interact**
button toggles sustained local interaction and highlights whenever local input
is active, including while holding the configured chord. Its modifiers are
consumed for application input and window-manager pointer shortcuts. The native
relay releases remote holds before local input activates; queued remote input
from an earlier ownership epoch is rejected. Remote capture remains available.

Blocked application content shows the cursor theme's **Not allowed** shape;
header controls and shell resize handles keep their own cursors. Ending local
interaction releases that surface's held keys and buttons, cancels touch,
gestures and finger scrolling, and sends pointer leave. Late physical release
events are consumed before routing input to another window. Other windows and
remote controllers retain their input state.

The controller configures the app's
viewport independently; moving or resizing the desktop frame changes only the
peek. Content stays at its natural logical size, anchored at the upper left and
clipped to the frame. Taking back control restores desktop input and resizing.
The header's **Fit** button sends a one-time application resize to the frame's
current content area, excluding borders and the titlebar. Later frame changes
continue to resize only the peek.
Scale-to-fit remains a later presentation improvement.

Grants expire on unmapping, occupant replacement, revocation or session exit.
Persistent slots retain their in-memory permission policy and control header
while empty. Permissions remain editable alongside the existing waiting
placeholder; a matching replacement automatically gets a fresh handle with
those permissions. Taking back control or globally revoking access clears the
remembered authorization. Restarting Weld also clears it. Ordinary managed
local application windows are eligible; hoisted windows must first be reclaimed.

Input and viewport operations return a receipt with `sequence`,
`stage: "effects_queued"` and the retained `frame_revision` at that boundary.
The native relay has handed off its effects; application processing follows
asynchronously. `capture` reads the latest retained composition. `capture-after`
waits for a composition revision newer than the receipt's checkpoint, then performs
the synchronized readback. Its deadline includes waiting and readback, and accepts
1–5000 milliseconds. A static app that produces no new frame times out. Animations
can produce an unrelated frame first, so callers still check the intended UI result
in the pixels. Frame revisions describe retained composition; inventory revisions
describe managed-window metadata.

Captures include contained popups and use a 64 MiB readback budget. At most four
captures/waits and 64 queued native operations are retained. The most recent 128
checkpoints per controlled application are available. Retired or expired checkpoints
fail explicitly. Expired operations fail, and callers must not automatically retry
uncertain input.

## Native testing loop

Launch one app through `weldctl launch -- PROGRAM ...`, then resolve it with
`window find --app-id APP_ID --occupied true --json`. Obtain local authorization
through the header or `window control`, and retain the copied shorthand for a
persistent slot. Use `window resolve` to inspect its current permissions and viewport.

```sh
receipt=$(weldctl --session weld-0 window click --control w12 --x 100 --y 100 --button 272 --json)
sequence=$(printf '%s' "$receipt" | jq -r .sequence)
weldctl --session weld-0 window capture-after --control w12 --sequence "$sequence" --timeout-ms 3000 --json
```

Check the resulting pixels, use `press-key`/`chord` to test keyboard behavior, then
resize the logical viewport or request a rendering scale and inspect the next
capture. After rebuilding an app, `window resolve w12` returns its fresh control
ID when the replacement occupies the remembered slot. A preceding incarnation's
input checkpoint is retired along with it.

`wait` returns the latest inventory after its revision changes. Unchanged
publication preserves the revision. Timeouts are errors and range from 1 to
20,000 milliseconds. Publications coalesce; callers reconcile the returned
snapshot. Host shutdown wakes pending waits. Plain headless hosting reports that
window management is unavailable.

## Schemas and generated interfaces

The implementation pins `server-less` at
`885c740e772edf98719cccf74fe5867b3fbcbb63` with CLI, JSON-RPC, MCP and JSON Schema
features. The service owns queries over an owned snapshot; the socket client
forwards calls to it. Macros generate service RPC dispatch and client CLI/MCP
interfaces. Schemars derives the field-level schemas from the wire types.

```sh
weldctl --session weld-0 window get --output-schema
weldctl --session weld-0 window get --input-schema
weldctl --session weld-0 window --manual --json
```

The last command exports a method-keyed catalogue containing descriptions,
input schemas and output schemas. These commands work without a running host.

The native control socket retains its four-byte little-endian length-prefixed
JSON records. A window query is a `Window` request containing JSON-RPC:

```json
{"Window":{"jsonrpc":"2.0","id":"example","method":"get","params":{"id":"0"}}}
```

The response contains `Window` with the matching JSON-RPC result/error and ID.
One window RPC request is accepted per connection. The existing same-user,
bounded control listener owns framing and connection lifetime.

Upstream's current OpenRPC generator uses syntactic type inference and emits
generic object schemas for custom types. It also camel-cases names independently
of the RPC dispatcher. The validated schema path here uses its Schemars-backed
CLI output. MCP's generated input schemas cover the simple string/integer
parameters used by this service; richer parameters need another contract check.

## MCP

Configure an MCP client to launch:

```sh
weldctl --session weld-0 mcp
```

The generated tools include inventory `list`, `find`, `get`, `resolve`, `wait`;
input `input`, `pointer`, `click`, `button`, `key`, `press_key`, `chord`, `type_text`, `scroll`;
viewport `resize`, `scale`; and `capture`, `capture_after`, `accessibility`, all prefixed `window_`.
MCP `window_list` defaults to control-enabled windows, including waiting persistent
slots. Pass `{"all_windows":true}` for the whole inventory. CLI/RPC `list` retains
the full inventory; `find` provides explicit application and control filters.
For equivalent filtered CLI queries:

```sh
weldctl --session weld-0 window find --controlled true --json
weldctl --session weld-0 window find draftship --occupied true --controlled true --json
```

Administrative grant creation and permission changes are exposed through the local
CLI/RPC and desktop binding. MCP control tools require an existing grant handle.
`server-less` supplies their descriptions and dispatch; the official Rust MCP
SDK supplies initialization, protocol negotiation and stdio lifecycle.
Results include structured content. Capture additionally returns an MCP PNG image
content block, allowing vision-capable agents to inspect the window directly.
Tool failures carry readable errors.
The bridge allows eight concurrent calls and performs blocking local RPC on
worker threads. An interrupted call's local RPC may continue until its bounded
wait completes. The endpoint runs as the same OS user.

## Accessibility inspection

Weld enables the session's AT-SPI `IsEnabled` flag when attaching its desktop
control service. Applications with an enabled accessibility bridge can then
publish their trees. This leaves the screen-reader setting unchanged.

Enable View access on a window, then use its copied control key:

```sh
weldctl --session weld-0 window accessibility --control w12 --max-nodes 64 --json
weldctl --session weld-0 window accessibility --control w12 --query save --role button --max-nodes 8 --json
weldctl --session weld-0 window accessibility --control w12 --max-depth 2 --json
```

The equivalent MCP tool is `window_accessibility`, with `control`, `query`,
`role`, `max_nodes` and `max_depth` arguments. Text search matches a substring
of the accessible name or application-supplied accessible ID; role matching is
exact. Both use lowercase comparison, and filters combine. Root depth is zero.
Results default to 256 nodes and depth 24, with maximums of 1024 and 24. Traversal
visits at most 1024 objects within a three-second request deadline and allows
two simultaneous inspections per host. Matching nodes carry role, name, states,
advertised actions and provider window-relative bounds. Bounds retain the
toolkit's coordinate units; check their relationship to the application's
viewport before using them as input coordinates.

`truncated` reports an incomplete traversal; `visited_nodes` distinguishes work
performed from matches returned. A node's `incomplete` flag marks unavailable
or shortened properties. IDs and parent IDs belong to one observation; filtered
results can omit a node's parent. These observations follow a changing UI and
should be refreshed before relying on old bounds. Actions are descriptive;
interaction continues through the existing pointer and keyboard operations.

The initial association requires exactly one managed native Wayland window for
the credentialed process and exactly one AT-SPI window owned by that same process.
Weld rechecks the association and original View grant before returning the tree.
Multiple windows report `ambiguous`; missing providers or unsupported native
credentials report `unavailable`. X11 association, cross-process embedded trees
and precise multi-window association require further identity support.

AccessKit-based applications must compile their Linux AT-SPI bridge. Draftship's
initial build with Blitz revision `d1b433edbc13da889228ae4f92908f7c28514019`
omitted `accesskit_unix` because `accesskit_xplat`'s default features were disabled.
Enabling `accesskit_unix` and its `async-io` backend exposed the tree successfully.
The provider currently supplies roles and text values, with mostly empty names,
missing bounds and no advertised actions;
see the pinned [workspace dependency](https://github.com/DioxusLabs/blitz/blob/d1b433edbc13da889228ae4f92908f7c28514019/Cargo.toml)
and [bridge features](https://github.com/DioxusLabs/blitz/blob/d1b433edbc13da889228ae4f92908f7c28514019/packages/accesskit_xplat/Cargo.toml).

## Remaining Draftship feedback

Whole-string UTF-8 typing is available for native Wayland windows. X11 text
delivery and explicit paste semantics remain follow-ups. Accessibility action
execution, bounded text/value inspection with sensitive-field handling, and
broader window association remain follow-ups. Mixed-version capability diagnostics
would make method/field mismatches during upgrades easier to identify.
Launch-bound window readiness, process filters, structured ownership refusals and
ownership-change waits, and mixed pointer/keyboard batches remain follow-ups.
The 2026-10-14 Draftship validation motivates these additions; the current frame
checkpoint supplies a bounded newer-frame observation for screenshot-based tests.

## Validation

```sh
cargo test --locked -p weldwm -p weld-control -p weld-remote-control -p weld-accessibility --lib
scripts/check-control-launch --nested
scripts/check-control-launch --nested --accessibility-app /path/to/zenity
scripts/check-control-launch --nested --xwayland
scripts/check-control-launch --nested --xwayland --text-app /path/to/zenity
```

Validated against a real nested Foot window: CLI/direct RPC result agreement,
unknown methods, invalid parameter types, field-level schema export, MCP
initialization, tool listing, inspection, missing-window errors, a pending wait
completed by a second application launch, and orderly shutdown.
The same runner verifies real terminal input through MCP, pointer input,
unchanged desktop focus, independently sized 800×600 and 1600×1400 application
viewports, native-size clipped desktop peeks, MCP PNG content, revocation and
fresh grant identities. It saves window and desktop PNGs beside its logs.
The optional Zenity probe validates a real GTK accessibility tree through CLI,
RPC and MCP, name/role filtering, depth/result limits, View revocation and
control-only MCP listing with an explicit full-inventory override. Its tree is
saved as `accessibility.json`. The validated GTK entry dialog exposes 17 nodes,
including the entry field and both buttons, with window-relative bounds.
The XWayland-enabled run also validated the existing X11 launch probe; its
shutdown still reports the previously recorded stale Wayland socket file after
confirming the endpoint is no longer listening.
