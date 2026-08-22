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
