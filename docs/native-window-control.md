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
disabled; that permission controls remote pixel capture.

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

The desktop frame initially remains a live, read-only peek. Hold the configured
modifier chord to use the focused window locally, or use the toggle binding to
keep local keyboard and pointer input enabled after releasing the chord. The
header shows **Local interaction** while takeover is active. The **Interact**
button toggles sustained local interaction and highlights whenever local input
is active, including while holding the configured chord. Its modifiers are
consumed for application input and window-manager pointer shortcuts. The native
relay releases remote holds before local input activates; queued remote input
from an earlier ownership epoch is rejected. Remote capture remains available.

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
input `pointer`, `click`, `button`, `key`, `press_key`, `chord`, `scroll`;
viewport `resize`, `scale`; and `capture`, `capture_after`, all prefixed `window_`.
Administrative grant creation and permission changes are exposed through the local
CLI/RPC and desktop binding. MCP control tools require an existing grant handle.
`server-less` supplies their descriptions and dispatch; the official Rust MCP
SDK supplies initialization, protocol negotiation and stdio lifecycle.
Results include structured content. Capture additionally returns an MCP PNG image
content block, allowing vision-capable agents to inspect the window directly.
Tool failures carry readable errors.
The bridge allows eight concurrent calls and performs blocking local RPC on
worker threads. An interrupted call's local RPC may continue until its bounded
wait completes. The endpoint runs as the same OS user. The current slice provides
pixel-based interaction; accessibility trees remain a separate follow-up.

## Remaining Draftship feedback

The next input slice is whole-string UTF-8 insertion/paste with an explicit
clipboard-preservation contract for the controlled seat. Accessibility snapshots,
role/name targeting and actions need application-to-AT-SPI identity validation.
Launch-bound window readiness, process filters, structured ownership refusals and
ownership-change waits, and general multi-step input batches remain follow-ups.
The 2026-10-14 Draftship validation motivates these additions; the current frame
checkpoint supplies a bounded newer-frame observation for screenshot-based tests.

## Validation

```sh
cargo test --locked -p weld-control --lib
scripts/check-control-launch --nested
scripts/check-control-launch --nested --xwayland
```

Validated against a real nested Foot window: CLI/direct RPC result agreement,
unknown methods, invalid parameter types, field-level schema export, MCP
initialization, tool listing, inspection, missing-window errors, a pending wait
completed by a second application launch, and orderly shutdown.
The same runner verifies real terminal input through MCP, pointer input,
unchanged desktop focus, independently sized 800×600 and 1600×1400 application
viewports, native-size clipped desktop peeks, MCP PNG content, revocation and
fresh grant identities. It saves window and desktop PNGs beside its logs.
The XWayland-enabled run also validated the existing X11 launch probe; its
shutdown still reports the previously recorded stale Wayland socket file after
confirming the endpoint is no longer listening.
