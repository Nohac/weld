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
opaque strings belonging to the running host. Geometry uses logical compositor
coordinates. Listings include application labels, occupancy, focus, visibility
and floating state. A non-null `control` handle permits agent view, keyboard,
pointer and viewport control of that particular application incarnation.

## Agent control and live peek

Enable control from the desktop using this Sway-config binding:

```text
bindsym $mod+Control+i weld agent toggle
```

The repository example includes it. Alternatively, select the managed window
from `window list` and grant control through the local administrative CLI:

```sh
weldctl --session weld-0 window control WINDOW_ID --enabled
weldctl --session weld-0 window list --json
```

Use the resulting `control` handle for operations:

```sh
weldctl --session weld-0 window resize CONTROL --width 1000 --height 700
weldctl --session weld-0 window click CONTROL --x 100 --y 100 --button 272
weldctl --session weld-0 window key CONTROL --keycode 30 --pressed
weldctl --session weld-0 window key CONTROL --keycode 30
weldctl --session weld-0 window capture CONTROL --json
weldctl --session weld-0 window control WINDOW_ID
```

The final command revokes control. `weldctl access revoke all` and the existing
`weld access revoke all` binding also revoke agent grants. After global revocation,
`weldctl access allow` explicitly enables new grants again.

One independent local-agent seat serves the granted windows. Operations preserve
the desktop's selected window. Keyboard codes are Linux evdev codes interpreted
through the configured host keymap. Pointer positions use window-local logical
coordinates; button codes are 272/273/274 for left/right/middle. Individual
`button` and `key` operations require matching release edges. Revocation releases
held input through the shared hoist relay.

The desktop frame remains a live, read-only peek. The agent configures the app's
viewport independently; moving or resizing the desktop frame changes only the
peek. Content stays at its natural logical size, anchored at the upper left and
clipped to the frame. Taking back control restores desktop input and resizing.
Scale-to-fit remains a later presentation improvement.

Grants expire on unmapping, occupant replacement, revocation or session exit.
Regranting creates a fresh handle. Persistent slots retain their layout while
their replacement applications require fresh control grants. Ordinary managed
local application windows are eligible; hoisted windows must first be reclaimed.

Input acknowledgements mean accepted native effects. Applications process those
events asynchronously. Capture returns the latest retained composition and its
revision; an immediate capture can therefore precede the app's next repaint.
PNG pixel dimensions and logical dimensions are both returned for coordinate
conversion. Captures include contained popups and use a 64 MiB readback budget.
At most four captures and 64 queued native operations are retained. Expired
operations fail, and callers must not automatically retry uncertain input.

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

The generated tools are `window_list`, `window_get`, `window_wait`,
`window_pointer`, `window_click`, `window_button`, `window_key`, `window_resize`
and `window_capture`. Administrative grant creation is exposed through the local
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
