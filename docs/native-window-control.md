# Native window inspection

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
and floating state.

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

The generated tools are `window_list`, `window_get`, and `window_wait`.
`server-less` supplies their descriptions and dispatch; the official Rust MCP
SDK supplies initialization, protocol negotiation and stdio lifecycle.
Results include structured content. Tool failures carry readable errors.
The bridge allows eight concurrent calls and performs blocking local RPC on
worker threads. An interrupted call's local RPC may continue until its bounded
wait completes. This slice exposes read-only inspection to the same OS user.
Per-window input, resizing and capture remain the next control slice.

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
The XWayland-enabled run also validated the existing X11 launch probe; its
shutdown still reports the previously recorded stale Wayland socket file after
confirming the endpoint is no longer listening.
