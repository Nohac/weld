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

The renderer-free assembly shared by `shell_main` and the `input_pipeline`
production-main control now disables renderer-dependent plugin families and
registers its shader loader. Measurements from before that initialization fix
are not directly comparable. Use the paced `renderer-main` case to include
the full renderer plugin assembly's main-world systems.

The render benchmark forces one composition per measured iteration so its
cases remain comparable; that is not a claim about normal demand policy. Check
the printed adapter before interpreting results. A CPU adapter validates the
path but is not representative of GPU performance.

### Paced real-client comparison

Compare diagnostic layers on the same production Wayland host and DMA-BUF
lifecycle, with a separate animated EGL client:

```sh
python3 scripts/profiling/paced-render --seconds 20 --width 2240 --height 1400
python3 scripts/profiling/paced-render --no-build --repeat 2 --perf /path/to/perf
python3 scripts/profiling/paced-render --no-build --executable /path/to/saved/benchmark --mode minimal
python3 scripts/profiling/paced-render --no-build --mode minimal --windows 3 --producer-hz 240 --input-hz 1000
```

The launcher builds an optimized release benchmark and the small C producer.
It requires Wayland/EGL/GLES development packages and `wayland-scanner` (or
`--scanner /path/to/wayland-scanner`). `--no-build` reuses those artifacts.
Results, settings, bounded logs and a post-measurement screenshot for each
presenter are saved below `target/validation/paced-render-*`.

`--mode all` runs the following cases. Repeats reverse their order on every
other pass. They isolate boundaries; differences are not an automatically
additive decomposition of production cost.

| Mode | Work added to the direct-rendered client workload |
| --- | --- |
| `surface` | Direct wgpu draw and shared core buffer manager |
| `empty-app` | Empty Bevy `App` main schedule |
| `core-app` | Bevy `MinimalPlugins` |
| `framework-main` | Framework main-world plugins, with renderer/core-pipeline/sprite-render/UI-render plugins disabled |
| `default-main` | Also Weld's model and one camera, with no client entities |
| `renderer-main` | Renderer installed, but only main-world work runs; no client entities |
| `state-direct` | Real AppShell ingress, surface state and promoted images; direct draw |
| `ui-direct` | Also normal SurfaceNode layout/picking; direct draw |
| `extract-direct` | State-direct plus extraction, deferred commands and temporary-entity retirement |
| `prepare-direct` | State-direct plus the normal render schedule, with camera-graph rendering gated off |
| `render-direct` | State-direct plus the whole empty Bevy renderer, then a direct draw |
| `minimal` | Normal AppShell and SurfaceNode rendering |
| `master` | Master policy, window UI and launch-free fixture config |

The first main-only cases keep input delivery in the raw presenter; their Bevy
apps receive no client events or input. State/extract/prepare/render-direct
have no mounted UI hit targets. Use `ui-direct`, `minimal` and `master` for
Bevy input/picking comparisons. The diagnostic direct draws select promoted
root images in surface-ID order and assume the fixture's opaque, upright,
uncropped, one-root-per-window content. They do not implement a general UI
renderer. Their first five frames run the normal renderer to settle startup;
use the default warmup or longer for steady-state measurements.

`prepare-direct` still runs the screenshot/readback tail; only
`RenderGraphSystems::Render` is gated. `render-direct` clears/blits through
Bevy and then clears/draws directly, so it intentionally does more GPU work than
`minimal`. These controls separate renderer scheduling from client UI, not
identical GPU command streams. Binding pruning on a live window unmap is not
exercised by the producer; a unit regression covers root selection at unmap,
promotion and out-of-order surface creation.
`extract-direct` must keep its no-mounted-UI configuration: buffers populated
by UI extraction are normally drained by preparation, which that case skips.

The live renderer and main-only controls share Weld's compositor plugin
selection. This retains UI/text, texture atlases, picking and the core renderer,
while omitting animation, gizmos and sprite/mesh-2D/tilemap presentation.
UI image-change extraction remains installed for cached texture bindings.
Use a saved pre-change executable to compare plugin selections without
rebuilding between runs. The separate EGL producer must already be built.

Optional `--perf` saves userspace instruction/cycle counters from an interior
window after `MEASUREMENT_START`. `counter-window.json` records its duration
and counts; `counters.csv` retains perf's raw output. The producer is excluded.
Unsupported/uncounted events or less than 99% counter coverage fail the run.
These counters cover a shorter interval than the whole-run CPU totals, and
exclude kernel instructions. Normal runs install a warning-level subscriber so
promotion failures remain visible without enabling Tracy.

The virtual output defaults to 60 Hz; clients default to 120 commits/second.
GPU work is bounded to three submissions in flight. Synthetic pointer motion
uses the normal input routing path. `--decorations server` adds an SSD workload;
the default requests no server decorations.

CPU percentages include all Weld process threads, expressed relative to one
core, and exclude the producer, warmup and final screenshot. Policy/render
phase timings are wall time. Commit ages start at bridge drain and end at CPU
render submission; they do not measure display latency. This offscreen test
uses scale 1 and native Wayland clients; physical presentation, Steam's XWayland
behavior and fractional scaling need separate measurements.

The [initial comparison](performance/paced-render-2026-10-03.md) records the
baseline and its limits.

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
