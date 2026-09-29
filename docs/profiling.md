# Profiling Weld

Weld uses `tracing` throughout its host and Bevy boundaries. The optional
`profiling-tracy` feature connects those spans to Tracy without scattering
feature gates through call sites.

Weld has runnable nested and Smithay-first standalone DRM backends. Focused DRM
probes remain lower-boundary validation tools rather than general profiling
hosts.

## Tracy capture

Run a repeatable scenario:

```text
scripts/profiling/three-firefox-videos --backend nested
scripts/profiling/shortcut-launch --backend nested --duration 30
scripts/profiling/shortcut-launch-with-initial-foot --backend nested
```

The scenario runner builds an optimized Tracy-enabled binary, prints any manual
action before taking over, starts a fresh Weld process, captures once, and
terminates the process group. Output defaults below `target/traces/`; use each
script's `--help` to select timing and paths.

Open a capture with:

```text
tracy-profiler target/traces/CAPTURE.tracy
```

Rank zones without the GUI:

```text
scripts/profiling/report-trace target/traces/CAPTURE.tracy
scripts/profiling/report-trace target/traces/CAPTURE.tracy --self --filter weld_
```

Use self time to separate wrapper spans from their children. Inclusive calloop
wait zones include time asleep and should not be interpreted as CPU use.
All native assemblies expose the shared zones `weld_calloop_wait_and_dispatch`,
`weld_host_client_ingress`, `weld_apply_policy_results`, and
`weld_flush_wayland_clients` on the `weld_profile` target. These replace the
former nested-only service zones and match the `--filter weld_` example above.

## Headless benchmarks

These `test-support`-gated benchmarks isolate application and render work from
a native backend:

```text
cargo bench -p weld-app --features test-support --bench input_pipeline
cargo bench -p weldwm --features test-support --bench shell_main
scripts/profiling/render-bench
```

`input_pipeline` reports raw ingress and Bevy schedule costs. `shell_main` adds
the standard window, presentation, SSD, float, and shortcut plugins.
`shell_render` constructs the real `AppShell` on a headless Vulkan device and
separates input ingress, surface ingress, main-world work, render submission,
and GPU completion.

The render benchmark forces one composition per measured iteration so its
cases remain comparable; that is not a claim about normal demand policy. Check
the printed adapter before interpreting results. A CPU adapter validates the
path but is not representative of GPU performance.

## Whole-process CPU profiles

### Full-output Blender orbit comparison

The orbit runner opens a fresh Sway workspace, makes the owned test window
fullscreen, records process CPU time, and restores the prior workspace when
the test workspace still has focus. Blender uses factory settings without
saving them. Pass the configuration explicitly:

```sh
scripts/profiling/orbit --config examples/master.sway.config
scripts/profiling/orbit --mode sway
```

Hold the middle mouse button and orbit during the printed capture interval.
Each run saves its settings, window/output geometry, CPU totals, frequency/GPU
samples and log under `target/validation/orbit-*`. CPU totals measure the Weld
or Sway process, excluding Blender. Changing focus stops the test. The default
capture lasts 20 seconds; `--no-build` reuses the existing Weld executable.

For automated replay, compile `pointer-replay.c` with `wayland-scanner`, a C
compiler, `pkg-config`, Wayland client development files and the
`wlr-virtual-pointer-unstable-v1.xml` protocol from wlr-protocols. For example,
after assigning `pointer_protocol` to that XML file:

```sh
pointer_build=$(mktemp -d)
wayland-scanner client-header "$pointer_protocol" "$pointer_build/virtual-pointer.h"
wayland-scanner private-code "$pointer_protocol" "$pointer_build/virtual-pointer.c"
cc -O2 -Wall -Wextra -Werror -I"$pointer_build" \
  scripts/profiling/pointer-replay.c "$pointer_build/virtual-pointer.c" \
  $(pkg-config --cflags --libs wayland-client) -o "$pointer_build/pointer-replay"
scripts/profiling/orbit --config examples/master.sway.config --no-build \
  --input orbit --hz 1000 --pointer-helper "$pointer_build/pointer-replay"
```

The persistent virtual pointer holds middle-click throughout `--input orbit`.
`--input motion` deliberately omits the button for an input-only control;
`--input idle` injects nothing. Replay checks focus before injection and every
quarter second during capture. Avoid interacting with other windows during it.

Useful separate controls are `--validation off` (disable the Khronos layer for
this process tree), `--client-vsync` (Mesa `vblank_mode=3` for Blender only), and
`--client-wayland-log` (count client protocol messages). Protocol logging
perturbs timing; use it separately from baseline CPU measurements.

`--perf /path/to/perf` records userspace stacks. `--profile optimized` builds
an optimized, symbolized binary **without Tracy** for baseline CPU measurements.
`--profile perf --trace` builds
an optimized, symbolized, frame-pointer-enabled Tracy binary and requires
`tracy-capture`. This can require a substantial build. The runner disables
Tracy's own sampling/symbol worker so external perf owns sampling, limits each
capture file to 256 MiB and the collector memory to 512 MiB, and disables core
dumps. Keep the executable for symbol resolution after capture. Trace timings
and userspace sample percentages complement process CPU totals; they do not
replace them. Detailed tracing can materially increase CPU use; compare against
the non-tracing binary before attributing CPU costs to the compositor.

The optimized variants are retained as `target/perf/weldwm-orbit-optimized`
and `target/perf/weldwm-orbit-perf` so switching features does not overwrite the
executable needed to symbolize a previous capture. `--no-build` reuses that
variant. Use `--jobs 6` to increase compilation parallelism; the default is two.
`threads.json` records per-thread CPU counters at both capture boundaries;
threads that exit during the interval can make their sum smaller than the
process total. `--perf-frequency` changes the sampling frequency (default 499),
and `--call-graph fp|dwarf` overrides unwinding (optimized defaults to frame
pointers, dev to 16 KiB DWARF stacks).
`--perf-stat` additionally writes `counters.csv` with process-attached task time
and userspace cycles, instructions, and cache misses. It requires `--perf`.
Check each counter's running percentage and unsupported/not-counted markers
before comparing results; process CPU accounting remains in `cpu.json`.
`--perf-event instructions:u` samples retired instructions instead of CPU time.
Weight those stacks by their recorded sample **period**, not sample count, and
do not label their percentages as CPU time. CPU samples now include the executing
CPU number, which helps distinguish workload changes from core placement on
heterogeneous machines. Both collectors are stopped with the owned test run.
Long dev captures with DWARF stacks can exceed the file bound and fail; use a
short capture or frame-pointer perf profile instead of raising limits blindly.

See the [Blender orbit investigation](performance/blender-orbit-2026-09-28.md)
for the measured buffer-release storm and proposed follow-up work.

For the matching manual DRM capture, switch to a text TTY and run:

```sh
scripts/profiling/drm-orbit
```

Hold middle mouse and orbit continuously until Weld exits. The launcher uses
the prepared non-Tracy optimized binary, factory Blender settings, disabled
Vulkan validation, disabled core dumps, and a 256 MiB per-file capture bound.
It delegates the six-second warmup, three-second sampling preflight, twenty-second
capture, process cleanup, and flamegraph generation to `run-perf`. Artifacts go
under `target/perf-traces`. Use `--build` to refresh the profiling binary with
six Cargo jobs before launching. Run it from the same development environment
as the nested tests; it does not take over an existing graphical session.

The [post-fix cost analysis](performance/render-costs-2026-09-29.md) separates
uninstrumented CPU totals, CPU sample attribution, and instrumented call counts.
The [excess-commit investigation](performance/excess-commits-2026-09-29.md)
compares work counts, instruction counts, and CPU time across client cadences.

### Generic workload runner

For workloads not explained by Tracy spans, use the generic perf runner:

```text
scripts/profiling/run-perf \
  --backend nested \
  --name WORKLOAD \
  --instructions 'Describe one repeatable action for the complete run.'
```

The runner builds the symbolized `perf` profile, captures userspace stacks,
records process user and system time, and writes reports and a flamegraph below
`target/perf-traces/`. It rejects lost or throttled samples and retains perf
stderr and metadata with the artifacts.

Read raw task-clock and process CPU values before percentages in a flamegraph.
If growth is primarily system time, userspace stacks are the wrong instrument;
kernel profiling requires a separate explicit system-policy decision.

Keep `target/perf/weldwm` and its dependency artifacts until analysis is
complete. Rebuilding that profile or running `cargo clean` can remove symbols
referenced by existing `perf.data` files.

## Physical-output validation

Use the Smithay compositor probe to validate direct DRM import, synchronization,
page flips, and VT recovery:

```text
scripts/run-smithay-drm-compositor-probe
```

Its Vulkan validation log proves correctness of the narrow output seam; it does
not measure production compositor performance. Use the production pacing trace
for the complete host:

```text
WELD_DRM_PACING_TRACE=1 scripts/run-weld-drm --seconds 60 foot
```

The trace reports hardware cursor assignment, Bevy composition, GPU wait,
vblank phase, and DRM sequence deltas. Log output can perturb timing, so compare
its conclusions with an uninstrumented release run.
