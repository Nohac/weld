# Remote debugging and screenshots

Weld exposes a deliberately restricted Bevy Remote Protocol endpoint for
development automation. It is opt-in and accepts loopback addresses only:

```text
cargo run -- --config examples/master.sway.config --remote-debug -- foot
cargo run -- --config examples/master.sway.config --remote-debug=127.0.0.1:16000 -- foot
```

The default main-world endpoint is `http://127.0.0.1:15702/`. Bevy 0.19 also
opens a render-world listener on fixed port `15703`; Weld replaces that
listener's method table with an empty table, but the socket still binds and can
collide with another process. Custom main-world addresses therefore cannot use
port 15703. Weld rejects non-loopback binds rather than exposing BRP to another
machine.

## Exposed protocol

The main endpoint contains exactly three methods:

- `rpc.discover`
- `world.get_resources`
- `world.write_message`

Broad Bevy methods for spawning entities or mutating components and resources
are removed from the dispatch table. The smaller table also omits
`registry.schema`, so generic BRP clients cannot discover reflected schemas.

Screenshot requests use this reflected message type:

```text
weldwm::debug::RemoteScreenshotRequest
```

Its value contains a monotonically increasing `request_id` and a `path` string.
Poll this reflected resource with `world.get_resources`:

```text
weldwm::debug::RemoteDebugStatus
```

A request has settled when `ready` and `idle` are true,
`completed_request_id >= request_id`, and `error` is empty. Invalid requests,
GPU failures, filesystem failures, and captures that cannot acquire a
presentable frame within ten seconds all complete with `error` populated.

The HTTP tasks and request systems advance through Weld's manually driven Bevy
`App::update`. The outer loop currently updates at least once per Smithay frame;
future event-loop changes must preserve that progress or BRP latency will grow.

## Client utilities

The standard-library-only `uv` project turns routine calls into stable commands:

```text
uv run --project tools/remote-debug weld-debug discover
uv run --project tools/remote-debug weld-debug status
uv run --project tools/remote-debug weld-debug screenshot target/weld-remote.png
```

Screenshots contain the completed client-plus-shell composition. Nested and DRM
presenters submit readback only on request. A bounded worker waits for GPU copy
completion, strips aligned row padding, and writes RGBA8 PNG data. Completion
wakes the native host and updates the request status after the file is written.
The same worker supplies the standard Wayland capture path described in
[Screenshots](docs/screenshots.md).
