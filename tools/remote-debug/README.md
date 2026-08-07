# Weld remote-debug tools

This small `uv` project wraps the three Bevy Remote Protocol methods exposed by
Weld. It has no runtime dependencies outside Python's standard library.

Start Weld with its loopback-only endpoint and an optional nested client:

```text
cargo run -- --remote-debug -- foot
```

Then inspect the endpoint or capture the complete client-plus-shell frame:

```text
uv run --project tools/remote-debug weld-debug discover
uv run --project tools/remote-debug weld-debug status
uv run --project tools/remote-debug weld-debug screenshot target/weld-remote.png
```

Pass `--url http://127.0.0.1:16000/` before the subcommand when Weld uses an
explicit main-world port. The render-world listener remains fixed at port
15703 and deliberately has an empty method table.
