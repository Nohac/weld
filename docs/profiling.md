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

The desktop default feature set includes these benchmark entrypoints. Explicit
fixture construction selects their application and render workloads:

```text
cargo bench -p weldwm -p weld-app --bench input_pipeline
cargo bench -p weldwm --bench shell_main
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
| `minimal-retained` | Minimal with scene-invalidated prepared Bevy UI reuse |
| `master-retained` | Master with the same prepared UI reuse |

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

### Initial retained UI experiment (2026-10-17, `cddaba7`)

The initial `test-support` presentation probe retained Bevy's prepared render entities,
UI geometry, phase ordering and material data. Content-only frames run a small
schedule that refreshes output attachments and surface bindings, executes the
existing Bevy render graph, and maintains the texture cache. Core DMA-BUF
promotion and completion remain around that submission. The main-world schedule
continues normally, and the complete output is redrawn. This experiment adds
no Bevy vendor changes.

The fixed-scene probe runs full preparation for the first 120 frames, then every
60 frames. Before a rebuild it retires the previous extracted entities.
Three early replay frames are read back and compared byte-for-byte with full
preparation of the same scene/buffers. Each also renders into a newly allocated
output texture to check backing-texture rotation. These assertions must finish
before `MEASUREMENT_START`; the launcher rejects a run whose validation spills
into measurement. Use at least four warmup seconds at 60 Hz.

```sh
scripts/profiling/paced-render --mode minimal-retained --warmup 4 --seconds 10
scripts/profiling/paced-render --no-build --mode master-retained --warmup 4 --seconds 10 --windows 3 --decorations server --input-hz 1000
```

Use `minimal` and `master` as the corresponding controls. When using an explicit
`cargo bench --no-run --profile release --bench paced_render --features test-support`
build, pass its executable with `--no-build --executable PATH`. The runner's
saved executable manifest belongs to its last successful managed build.

Measurements on Radeon 880M/RADV, kernel 7.2.8, optimized release, validation
disabled, 1920×1080 at 60 Hz, and real EGL DMA-BUF clients at 120 Hz each:

| Workload | Normal CPU, % of one core | Replay CPU, % of one core | Mean reduction |
| --- | --- | --- | --- |
| One surface, minimal shell | 9.399 / 8.800 | 6.198 / 6.300 | 31% |
| Three SSD windows, Master, 1000 Hz input | 12.799 / 10.899 | 9.900 / 8.500 | 22% |

Each workload ran sequentially in normal/replay/replay/normal order, with four
seconds warmup and ten seconds measurement. Compilation was finished before
measurement. Both paths maintained approximately 60 compositions/second and
the expected 120/360 total commits/second. Render-call wall time averaged
811→444 µs for the minimal fixture and 838→505 µs for the Master fixture.
CPU variation between repeats is visible above; these are offscreen fixture
results, with physical scanout and end-to-end latency left for later validation.

Evidence directories under `target/validation/`, in execution order:

- Minimal: `paced-render-uanm_uaq`, `paced-render-4xtlr4qd`,
  `paced-render-clj4y2kg`, `paced-render-poierscg`.
- Master: `paced-render-jk3jwi42`, `paced-render-mlhf7mhl`,
  `paced-render-ps3fkhk3`, `paced-render-qbgb5m8p`.

The production follow-up below replaces periodic rebuilds with scene
invalidation and preserves Bevy's full frame lifecycle, including device-error
handling. The small replay schedule above served as a performance lower bound.

Validation also includes a GPU regression with a rounded, clipped surface and
an overlapping translucent UI node. It compares replay/full preparation and
rotated targets before and after an explicit resize rebuild and node teardown.
An entity-lifetime test checks that retained entities survive replay and are
retired without removing persistent render entities. The benchmark assembly's
stale watermark-plugin reference was updated to the current output-topology
plugin so both control and replay use the same runnable assembly.

### Scene-invalidated UI preparation

The normal shell now reuses prepared UI batches when UI/camera components,
structure, relevant assets and GPU-image registrations remain unchanged.
`minimal`/`master` explicitly disable reuse for comparison; their `-retained`
counterparts exercise this production path. Each retained benchmark requires
pixel-equivalent full preparation, correct output-texture rotation and evidence
that preparation was actually reused before measurement starts.

Both paths execute Bevy's normal render-app lifecycle. Stable frames skip UI
extraction, queueing, sorting and bind-group preparation; asset uploads, view
setup, device-error handling, GPU drawing and completion continue normally.
Surface bindings refresh each frame. Native acquisition/release ordering stays
around the submission. This preserves overlapping translucent surfaces and
rounded clipping while saving CPU preparation work.

Validation found three sources of false invalidation: a mutable material-handle
coercion, always-rewritten Bevy camera propagation intermediates, and no-op
image-registry resource scopes. Stable handles now retain their ticks; the
detector watches final computed camera components; empty/already-installed
registry operations return before entering a resource scope. Bevy 0.19 resource
scopes reinsert resources with a fresh change tick even without a mutation.

GPU regressions cover rounded clipping, translucent overlap, live SHM pixel
updates, text/glyph changes, resize, node teardown, backing-target rotation,
ordinary/tiled images, fractional pixel snapping and multiple output passes.
Policy tests cover removals across intervening main updates and expired asset
events. `RetainedUiStats` exposes prepared/reused counts; use
`RUST_LOG=warn,weld_render_cache=trace` to inspect individual invalidations in a
separate diagnostic run. See [Architecture](architecture.md) for fallback
controls when adding custom render pipelines.

Production-path measurements used the same optimized release, RADV device,
1920×1080/60 Hz output and 120 Hz producers as the initial experiment. These
ABBA runs used four seconds warmup and ten seconds measurement, with all builds
completed first:

| Workload | Normal CPU, % of one core | Retained CPU, % of one core |
| --- | --- | --- |
| One surface, minimal shell | 8.498 / 9.100 | 7.300 / 8.900 |
| Three SSD windows, Master, 1000 Hz input | 9.600 / 9.400 | 12.700 / 10.300 |

The minimal mean improves by 8%, but the Master CPU totals regress in this
sequence. A second Master ABBA sequence, with twelve seconds measurement and
hardware counters over an interior ten-second window, gives:

| Metric | Normal | Retained |
| --- | --- | --- |
| CPU, % of one core | 12.083 / 11.332 | 11.500 / 11.249 |
| Userspace instructions, million/s | 122.256 / 122.919 | 111.114 / 111.959 |
| Userspace cycles, million/s | 240.705 / 235.879 | 235.590 / 207.402 |

Instruction rate falls 9.0%; CPU time falls 2.8% in the counter sequence.
Render-call wall time averages 837→748 µs there. All cases sustain approximately
60 compositions/s and 120/360 commits/s. The counter evidence supports less CPU
work, while these CPU-time samples do not establish a repeatable multi-window
speedup. The earlier small-schedule prototype's 22–31% savings should not be
attributed to this safer production path. Physical DRM latency and interactive
desktop testing remain the next checks.

Evidence directories under `target/validation/`, in execution order:

- Minimal: `paced-render-79sgw50q`, `paced-render-iav0ja_g`,
  `paced-render-2myt16e7`, `paced-render-xw_l_ujb`.
- Master: `paced-render-p4a88qq3`, `paced-render-obb0hzlm`,
  `paced-render-8fdw2z5l`, `paced-render-xefon__p`.
- Master counters: `paced-render-0ap7nqmi`, `paced-render-o6pzf8df`,
  `paced-render-ej7ywp6f`, `paced-render-9nekcdbw`.
- Nested Foot/Grim/Grimshot/clipboard validation: `screenshot-vkhgvmvd`.

The nested capture checks pass, including cancellation and concurrent captures.
Their log also reports `VUID-vkAcquireNextImageKHR-fence-10066`; the same warning
is present in the October 11 `screenshot-nt09evjt` and `screenshot-c1wxiiva`
logs and the September 28 profiling report. That acquire-fence issue remains a
separate validation finding.

### Content-only application scheduling

Native composition now admits surface content separately from the main Bevy
schedule. Stable DMA-BUF updates skip window policy, UI layout and input
reconciliation. Structural changes, input, explicit policy wakes, resize
acknowledgements and asset-processing work select the full update path.
`RetainedUiSettings.enabled = false` selects full application/render preparation
for the benchmark control and custom renderer integrations.

The following optimized ABBA comparison uses two SSD EGL clients at 120 Hz
each, a 1920×1080/60 Hz output, four seconds warmup and ten seconds measurement.
The baseline binary is `ed9691c`'s scene-invalidated retained renderer. Both paths
retain render batches; this comparison isolates the application-update split.

| Metric | Previous retained path | Content-only path |
| --- | --- | --- |
| CPU, % of one core | 10.393 / 10.900 | 5.899 / 7.400 |
| Userspace instructions, million/s | 75.361 / 75.598 | 55.363 / 55.470 |
| Full application updates during measurement | every composition | 0 / 0 |

Mean CPU time falls 37.5%, and instruction rate falls 26.6%. All runs retain
approximately 60 compositions/s and 240 commits/s. Artifacts, in order:
`paced-render-nga8mx4q`, `paced-render-bti19f2g`, `paced-render-7gj51ijk`,
`paced-render-m5rzmvwt` under `target/validation/`. The preserved baseline binary
is `target/validation/paced-render-build/before-content-path`.

The fixture also supports actual alternating focus using its F7/F8 bindings,
and F6 to select tabs. `--focus-hz 4 --windows 2` changes focus four times per
second; add `--tabbed` to exercise visibility and tab-header changes. Use
`master` or `master-retained` with the supplied benchmark config. `FOCUS` log
records verify that the managed selection really changed; the launcher rejects
a focus run without those transitions. `POLICY` records the main-update count.

| Scenario | Full-update control CPU | Content-only + retained CPU | Main updates, control → optimized |
| --- | --- | --- | --- |
| Side-by-side focus, 4 Hz | 10.999% | 6.998% | 599 → 80 |
| Tab switching, 4 Hz | 11.996% | 7.399% | 600 → 80 |

These single pairs compare the combined main/render optimization against full
preparation in the same binary. They use the same 1080p60/two-client workload
and warmup/measurement periods as above; builds were finished before timing.
Side-by-side artifacts: `paced-render-s5oq_n1b`, `paced-render-_9ood74g`.
Tabbed artifacts (optimized first): `paced-render-mpw0avud`,
`paced-render-cxf3oaca`. Retained runs pass byte-equivalent full-render and
rotated-target checks before measurement. The real nested capture regression
passes in `screenshot-31zgqnq7`. These remain offscreen fixture measurements;
physical browser-video cadence requires the separate presentation-time work.
After adding the same-size resize acknowledgement guard, final five-second
smoke runs still report zero steady-content policy updates and 40 updates for
4 Hz tab switching, with all GPU comparisons passing (`paced-render-o8cjmt1c`,
`paced-render-3j8ea_zo`). The final focused suites pass 358 tests, including
window, floating and hoist resize settlement.

Adding commit/layer sampling for `wp_presentation` preserves that scheduling
behavior. A final optimized ABBA run with the same steady workload compares the
saved content-only binary with the feedback-enabled build:

| Metric | Before feedback bookkeeping | With feedback bookkeeping |
| --- | --- | --- |
| CPU, % of one core | 7.200 / 6.600 | 6.800 / 7.500 |
| Userspace instructions, million/s | 54.863 / 55.372 | 57.309 / 57.164 |
| Full application updates | 0 / 5 | 0 / 0 |

Both paths sustain 60 compositions/s and 240 commits/s. Sampling adds about
3.8% instructions; mean CPU changes from 6.90% to 7.15% of one core in this run.
Artifacts in ABBA order: `paced-render-68pu6f6w`, `paced-render-sqy704_4`,
`paced-render-dqn5ssog`, `paced-render-ccnxacy8`. These producers exercise sampling
bookkeeping; real protocol delivery is covered by the Wayland socket tests in
[presentation feedback](drm-presentation.md#presentation-feedback). The final
nested capture/clipboard regression passes in `screenshot-onjndtv9`; it still
logs the previously recorded WGPU acquire-fence validation warning.
The final 4 Hz tab-switch run (`paced-render-oszd7l41`) uses 7.100% CPU,
59.898 compositions/s and 80 full updates over ten seconds, with byte-equivalent
full-render and rotated-target checks passing.

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
