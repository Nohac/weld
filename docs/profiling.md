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
Bevy's Tracy feature also enables the `profiling` crate's tracing backend by
feature unification. Existing Smithay, wgpu, and related dependency annotations
therefore appear in the same capture without Weld enabling another profiler.

Weld's custom zones are ordinary `tracing` callsites rather than Tracy-specific
code. They are always compiled, but normal `info` logging statically disables
their `weld_profile` trace target at each callsite. Profiling still adds
instrumentation and is not intended for ordinary development builds.

## DRM runtime counters

For a low-overhead rate check without a Tracy build, enable the standalone DRM
host's interval counters:

```text
WELD_DRM_METRICS=1 cargo run --release
```

The host emits one structured `weld_metrics` info event after each elapsed
second in which ordinary backend activity wakes it, plus a final partial
interval during shutdown. Metrics do not install a timer and therefore do not
wake an otherwise idle compositor. `interval_ms` is the actual duration covered
by each report; divide counts by it when comparing rates.

The fields distinguish work at each pacing boundary:

- `loop_iterations` counts completed outer DRM host iterations.
- `input_batches` counts host dispatch iterations containing at least one raw
  input event. It is not an internal kernel or libinput wake count.
- `raw_input_events` and `forwarded_client_events` compare converted input with
  immediate delivery attempts to the focused client.
- `surface_events` counts events crossing into the application, while
  `surface_commits` counts their `TreeSnapshot` subset as Weld's observable
  proxy for client commits.
- `main_updates` counts Bevy main-world advances, and `redraw_promotions`
  counts advances whose `RequestRedraw` messages promoted them to composition.
- `compositions`, `presentation_batches`, and `output_presentations` separate
  Bevy rendering from accepted physical presentation submissions and
  multi-output fanout.
- `cursor_motion_refreshes`, `cursor_sync_refreshes`, and
  `cursor_vblank_retirements` identify the driver of cursor work. Their matching
  `*_compositions` fields count refreshes that required the GPU cursor overlay
  to be recomposed. The synchronization pass is demand-driven by a main-world
  advance, backend lifecycle event, pending cursor image, or due animation; it
  does not poll on every input-driven host iteration. A temporarily deferred
  cursor-only atomic commit retires on vblank; a transiently deferred cursor
  update retries on later cursor work or VT recovery.
- `hardware_cursor_commits` counts successful Smithay atomic submissions that
  include changed cursor-plane state, whether cursor-only or merged into a
  primary frame. Compare it with the refresh counters to measure coalescing.
  `hardware_cursor_fallbacks` counts new oversized-cursor or permanent-failure
  outcomes, excluding ordinary hiding, VT suspension, and temporary deferral.

The interval counters aggregate the same host boundaries exposed in Tracy by
`DRM runtime event batch`, `host surface batch`, and `ECS result batch`. Use the
counters to establish rates first, then use those zones or the Bevy system
report to attribute the cost inside a busy boundary.

## Reading Weld zones

Weld's profiling zones follow the host boundary. Backend-specific
`*_calloop_wait_and_dispatch` zones include the backend wait as well as Smithay
callback dispatch. Host input and surface ingress zones cover batches crossing
into the app, `*_apply_ecs_results` covers effects crossing back to Smithay,
and `host_launch_client` covers both startup and shortcut-requested launches.
`drm_host_input_event` is deliberately per event because DRM runtime events are
interleaved in one queue; `nested_host_input_ingress` remains batch-scoped.
`drm_libinput_convert_event` runs inside Smithay's `process_events` zone and
separates Weld conversion and queueing from native libinput dispatch.

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

The DMA-BUF stable-binding change should be evaluated after the client buffer
pools have warmed. Compare equivalent three-video captures using Tracy's
statistics for bind-group creation, command-encoder finish, and queue submit,
plus process CPU over the same steady-state interval. Bind-group creation may
occur once for each live buffer/material/parameter combination during warm-up;
steady rotation should then reuse those entries. Record measured results here
only after a controlled before/after capture.

## Repeatable scenarios

The scenario scripts build Weld, prepare clients, print the manual actions and
timeline, collect one trace from a fresh Weld process, and tear down the whole
process group:

```text
scripts/profiling/three-firefox-videos
scripts/profiling/shortcut-launch --duration 30
scripts/profiling/shortcut-launch-with-initial-foot
scripts/profiling/pointer-motion-suite
scripts/profiling/pointer-motion --rate still
scripts/profiling/pointer-motion --rate slow
scripts/profiling/pointer-motion --rate rapid
scripts/profiling/pointer-motion --rate rapid --focused-client
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

Each pointer-motion run uses one fixed action, because instructions printed by
the launching terminal are hidden while Weld owns the VT. Once Weld is visible,
perform the selected action continuously until it exits. The empty `still` run
establishes the idle floor. Compare empty `slow` with empty `rapid` to separate
fixed libinput wake cost from per-event cost, then compare empty `rapid` with
focused `rapid` to isolate focused-client protocol delivery.

`pointer-motion-suite` runs those four comparisons automatically. It announces
each fixed action and waits eight seconds before Weld takes over the VT, then
continues to the next action after Weld and its trace collector exit.

Pointer overlay changes currently request a full Bevy composition, clamped to
the output refresh interval. Both nonzero motion rates should therefore
saturate composition at the refresh rate, making `slow` versus `rapid` the
useful input-path comparison. `still` versus `rapid` also includes the cost of
roughly one composition per refresh and must not be attributed entirely to
input. Seeing `weld_app_advance_composition` and render zones in both motion
traces is expected.

The controlled touchpad and external-mouse results, allocation audit, and
agreed optimization order are recorded in
[Input performance](input-performance.md).

In the Tracy UI, use self-time and zone counts rather than the inclusive time
of `drm_calloop_wait_and_dispatch`, which includes time asleep inside
`calloop.dispatch`. The libinput callback fires within that wait zone, so its
child and self times remain meaningful.

Several dependency sources use the zone name `process_events`. Include source
columns when separating Smithay's libinput, DMA-BUF, and libseat sources:

```text
tracy-csvexport -s $'\t' -f process_events TRACE | cut -f1-4,6-7
tracy-csvexport -s $'\t' -e -f process_events TRACE | cut -f1-4,6-7
scripts/profiling/report-trace TRACE --self --filter drm_libinput_convert_event
scripts/profiling/report-trace TRACE --self --filter drm_host_input_event
scripts/profiling/report-trace TRACE --self --filter drm_runtime_event_drain
scripts/profiling/report-trace TRACE --self --filter drm_flush_wayland_clients
scripts/profiling/report-trace TRACE --self --filter weld_app_advance_composition
```

For each motion trace, divide `drm_libinput_convert_event` count by the
libinput-source `process_events` count to estimate events per wake. Two nonzero
motion rates distinguish fixed wake cost from per-event cost; the still trace
only establishes the idle floor. A historical, uncontrolled video trace
reported 24.619 microseconds of libinput `process_events` time per wake at about
38 wakes per second. That old zone had no recorded child, so its inclusive and
self times were both 57.955 milliseconds. For contrast, the same trace's
DMA-BUF `process_events` zone measured 74.431 milliseconds inclusive and
25.970 milliseconds self. Compare the historical libinput value with the new
inclusive value, not the new self value. With the current instrumentation, the
new inclusive value also equals its self time plus the total inclusive time of
`drm_libinput_convert_event`, because that conversion span is its only recorded
child. Recheck that identity if another child span is added later.

`drm_host_input_event` is now a child of `drm_runtime_event_drain`. When
comparing against traces captured before that span existed, compare the drain's
inclusive time, or add its self time to its child time; comparing self time
alone would manufacture an apparent improvement.

The controlled traces showed why the historical mean must not be extrapolated.
Rapid touchpad motion produced roughly 130 to 146 wakes per second at 34 to 39
microseconds per wake. The external mouse produced about 744 wakes and 925
converted events per second, but its mean libinput wake cost fell to about 9
microseconds. Device event shape and batching materially change the per-wake
cost. Use an uninstrumented sampling or allocation profiler before assigning a
production CPU percentage to these Tracy timings.

The fixed mapping-settle budget is a temporary integration margin. Bevy does
not currently expose a reliable signal that all deferred main-world, layout,
asset, extraction, and render-world work for a newly mapped surface has
converged. Replace the fixed budget when such a signal is available.

## Headless main-schedule benchmarks

Three `test-support`-gated Cargo benchmarks isolate application and rendering
work without a native backend. They are excluded from ordinary Weld builds.

```text
cargo bench -p weld-app --features test-support --bench input_pipeline
cargo bench -p weldwm --features test-support --bench shell_main
scripts/profiling/render-bench
```

`input_pipeline` separates raw batch ingress and the `First`, `PreUpdate`,
`Update`, `PostUpdate`, and `Last` schedules. `shell_main` adds Weld's normal
window, presentation, SSD, float, and shortcut plugins, then compares zero,
one, and three retained windows. Set `WELD_BENCH_FRAMES` to change the default
10,000 measured updates.

These are wall-clock microbenchmarks in Cargo's optimized bench profile. Use
them for relative comparisons and subsystem elimination, not as a replacement
for end-to-end DRM profiling.

`shell_render` constructs the real [`AppShell`](../crates/weld-app/src/shell.rs)
against a headless Vulkan device and drives the same host contract as a native
backend: input ingress, main-world advance, Bevy extraction, and composition
submission. Its cases compare no input and a burst of sixteen synthetic pointer
motions across an empty scene, a mapped synthetic client without new commits,
and the same client sending retained commits. The mapped-client case without
retained commits also includes an interleaved workload where `BTN_TASK`
transitions act as ordering
barriers without producing Bevy pointer or mouse-button messages. This keeps a
non-coalescible input baseline alongside the motion-coalescing workload. The
initial client image uses SHM once; measured commits retain that image and
therefore do not include per-frame pixel copying. Each case reports input
ingress, surface ingress, the main schedule, CPU-side render submission, and
the subsequent GPU completion wait separately.

Set `WELD_RENDER_BENCH_FRAMES` and `WELD_RENDER_BENCH_WARMUP` to override the
default 600 measured and 30 warm-up frames. The benchmark always forces one
composition per measured iteration so input and retained-commit cases remain
directly comparable; this does not claim that those events should request a
composition in normal operation.

Check the printed adapter and device type before interpreting results. A
`device=Cpu` adapter such as llvmpipe validates the bridge but does not measure
hardware rendering. This benchmark also deliberately stops above calloop,
Smithay protocol dispatch, client forwarding, KMS acquisition, and physical
presentation. If its host-boundary timings are cheap while a live compositor
is expensive, use Tracy to investigate those backend stages.

## Whole-process CPU flamegraphs

Use the perf suite when Tracy zones and source inspection do not explain live
CPU utilization:

```text
scripts/profiling/pointer-motion-perf-suite
```

Run it from a real TTY. It builds the `perf` Cargo profile once, then records
three equal-duration DRM sessions: a still pointer, rapid pointer motion, and a
second still pointer. Each prompt appears before Weld takes over the TTY and
shows the exact action and timing before its eight-second countdown. Continue
the requested action through the three-second loss probe and the main capture,
until Weld exits. Options such as
`--duration`, `--frequency`, `--mmap-pages`, and `--stack-bytes` apply
identically to all three runs.

The `perf` profile inherits release code generation, does not enable Tracy, and
adds full DWARF information. `run-perf` rejects a binary linked to
`libbevy_dylib`, checks free space before building, and samples `cpu-clock:u` at
a fixed frequency. This event makes userspace sample counts proportional to
userspace CPU time without depending on CPU frequency. The script also records
`task-clock` plus `/proc` user and system CPU ticks. It uses a fixed perf ring
size for every suite member and refuses comparisons when perf reports lost or
throttled samples, a capture has no samples, task-clock disagrees with the
process CPU ticks, or the two still baselines differ by more than ten percent
in task-clock or sample count. Task-clock drift also has a 50 ms absolute floor
for scheduling and startup noise, while sample drift has a 20-sample floor
matching the sampler-correlation threshold. These floors keep tiny nonzero idle
measurements from becoming meaningless large percentages. The actual
task-clock event name and perf stderr remain in the artifact set so a NixOS
permission fallback is visible.

A settled compositor can legitimately accumulate no userspace samples. Weld
therefore corroborates an empty capture against `/proc` user CPU ticks: zero
samples are accepted only when the measured user time predicts fewer than 20
samples at the selected frequency. Such a run retains task-clock and
user/system accounting but has `flamegraph_available=false`. A nonempty capture
must contain meaningful multi-frame DWARF stacks before its SVG is accepted.
Symbol and differential comparisons require sampled still and rapid captures;
raw CPU accounting remains comparable when an idle baseline has no flamegraph.
On the verified two-output DRM baseline, the empty compositor accumulated zero
samples and zero process CPU ticks over the capture and perf reported
`task-clock` as `<not counted>`. Weld records that as corroborated idle with a
normalized zero task-clock. The CPU cost under investigation is therefore
motion- or client-driven; the still runs are controls, while the rapid run is
the substantive flamegraph.

The suite writes artifacts below a timestamped directory in
`target/perf-traces/`; standalone `run-perf` calls use timestamped flat files
there unless `--output-base` selects another location. The primary files are:

- `rapid.svg`: the full rapid-pointer userspace flamegraph;
- `rapid.report.txt`: flat self-time by thread, shared object, and symbol;
- `rapid.children.txt`: inclusive call-tree costs;
- `rapid.stat.txt` and `rapid.cpu.txt`: total task time and user/system split;
- `comparison.txt`: raw per-run sample, task-clock, user-time, and system-time
  values plus the baseline drift decision;
- `still-to-rapid.ratio.txt` and `still-to-rapid.delta-abs.txt`: perf symbol
  comparisons; and
- `still-to-rapid.diff.svg`: an absolute-sample differential flamegraph. It is
  generated without `difffolded.pl -n`, because the captures have equal
  duration and frequency. Frame width follows the rapid profile, while warmer
  colors indicate growth from still to rapid and cooler colors indicate a
  reduction.

Read `comparison.txt` before the flamegraph. Percentage-only symbol columns do
not reveal a uniform increase in CPU work; raw sample counts and task-clock do.
If rapid motion increases task-clock primarily through `stime`, the userspace
flamegraph is the wrong instrument. Kernel stacks then require an explicit,
temporary policy change to `kernel.perf_event_paranoid=1`; the scripts never
change that setting. Ask before changing the NixOS sysctl or invoking `sudo`.

The scripts resolve `perf` and the Brendan Gregg FlameGraph tools from the
current Nix registry with `nix build --no-link` when they are not already in
`PATH`. The first run may therefore need network access. Metadata records the
exact kernel, perf version, sysctls, event, frequency, ring size, stack size,
thread count, and tool paths. Sampling still has overhead, and the fixed
still-motion-still order can have thermal bias; the second still run is the
guard against treating that drift as pointer cost.

Keep `target/perf/weldwm` and its dependency artifacts until analysis is
complete. Rebuilding that profile or running `cargo clean` can remove the
symbol files referenced by `perf.data`. The general runner can profile another
manual workload with:

```text
scripts/profiling/run-perf \
  --name WORKLOAD \
  --instructions 'Describe one repeatable action for the complete run.'
```

If an abnormal SIGKILL leaves the VT in graphics mode, switch to another VT and
back with `chvt`, or reboot if the display session cannot be recovered.

## Automated trace reports

Rank individual ECS systems or other Tracy zones without opening the GUI:

```text
scripts/profiling/report-trace target/traces/capture.tracy
scripts/profiling/report-trace target/traces/capture.tracy --count 40 --self
scripts/profiling/report-trace target/traces/capture.tracy --filter weld_
```

The default report selects Bevy `system{...}` zones and ranks inclusive total
time. `--self` removes nested-zone time, which is useful for wrapper systems
such as Bevy's main-schedule runner.
