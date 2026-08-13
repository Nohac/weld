# Profiling Weld

Weld owns the process-wide `tracing` subscriber because compositor and backend
work exists outside Bevy's app. The opt-in `profiling-tracy` feature adds
Tracy's layer to that subscriber and enables Bevy's ECS, render, and debug
instrumentation without restoring Bevy's `LogPlugin`.

## Interactive capture

The shared Rust shell provides the Tracy GUI and command-line capture tools at
the version matching Weld's profiling client. Start the GUI, then run an
optimized Weld from another terminal:

```text
tracy
```

```text
TRACY_PORT=18086 cargo run --release --features profiling-tracy -- foot
```

Profiling builds do not force-link the development `bevy_dylib`, including in
the debug profile, because Tracy's native client must live in the final
executable. Ordinary debug builds retain the faster dynamic-link workflow.
Tracy's native client is written in C++, so the profiling executable also needs
a C++ standard-library runtime such as `libstdc++.so.6` available to the loader.
The explicit data port avoids an unrelated TCP listener on Tracy's conventional
8086 port. Tracy's GUI discovers the advertised data port automatically; pass
`-p 18086` to `tracy-capture` when using the command-line collector.

`RUST_LOG` controls formatted terminal output but not Tracy capture contents.
Tracy receives `info`-level Bevy and dependency spans plus Weld's dedicated
`weld_profile` trace target; frame-marker events remain hidden from the
terminal. Prefer simple level and target directives in `RUST_LOG` while
profiling, since span- or field-based directives require more dynamic callsite
filtering. On devices without the required timestamp-query features, CPU and
ECS spans remain available but the GPU `RenderQueue` timeline is omitted.

Weld's custom zones are ordinary `tracing` callsites rather than Tracy-specific
code. They are always compiled, but normal `info` logging statically disables
their `weld_profile` trace target at each callsite. Profiling still adds
instrumentation and is not intended for ordinary development builds.

## Reading Weld zones

Weld's profiling zones follow the host boundary. Backend-specific
`*_calloop_wait_and_dispatch` zones include the backend wait as well as Smithay
callback dispatch. Host input and surface ingress zones cover batches crossing
into the app, `*_apply_ecs_results` covers effects crossing back to Smithay,
and `host_launch_client` covers both startup and shortcut-requested launches.

For launch and mapping diagnosis, follow `host_launch_client`,
`host_accept_wayland_client`, `drm_host_surface_ingress`,
`weld_surface_created_ingress`, `weld_surface_snapshot_ingress`, the Bevy
surface/window systems, `weld_render_composition`, `drm_present_frame`,
`acquire_surface_texture`, and `encode_submit_present`. The lightweight Created
zone is a presence marker rather than a cost measurement. An accepted client
without a Created zone localizes the next investigation to Wayland global
binding and xdg-toplevel creation, which requires protocol logging.

`weld_render_composition` contains DMA-BUF preparation, Bevy rendering, and
retirement. Presentation separately shows `acquire_surface_texture` and
`encode_submit_present`, with DRM work on the `weld-drm-presenter` thread.
Bevy's frame mark is emitted by `render_composition`, not by physical
presentation, so presentation work may appear at the beginning of the next
Tracy frame.

## Repeatable scenarios

The scenario scripts build Weld, prepare clients, print the manual actions and
timeline, collect one trace from a fresh Weld process, and tear down the whole
process group:

```text
scripts/profiling/three-firefox-videos
scripts/profiling/shortcut-launch --duration 30
scripts/profiling/shortcut-launch-with-initial-foot
```

Use each script's `--help` for backend, warmup, output, and scenario-specific
options. The Tracy client buffers from process startup, so connecting the
collector later does not remove startup from the trace. Scenarios use a
two-second startup margin by default. Pass `--warmup` when a workload needs more
time to reach steady state, or `--warmup 0` to capture immediately. The Firefox
scenario retains profiles and browser caches between ordinary runs; pass
`--fresh` when a disposable cold profile is part of the measurement.

The remote-debug-enabled `composition-handoff` scenario exists for diagnosing
offscreen Bevy handoff. Remote debugging wakes the main world at a bounded
maintenance interval and on other host traffic, but does not itself request
continuous composition.

The fixed mapping-settle budget is a temporary integration margin. Bevy does
not currently expose a reliable signal that all deferred main-world, layout,
asset, extraction, and render-world work for a newly mapped surface has
converged. Replace the fixed budget when such a signal is available.
