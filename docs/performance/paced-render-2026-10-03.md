# Paced presentation baseline — 2026-10-03

The animated Steam workload showed roughly 20% of one CPU core in release Weld.
To isolate host, rendering integration and WM costs, `paced_render` drives the
production host with a separate GPU-backed EGL producer and interchangeable
presenters. The original Steam profile and this controlled workload are
different experiments.

## Matched baseline

Run `target/validation/paced-render-_ngbqbu2`: release opt-level 3, RADV 880M,
Khronos validation disabled, 2240×1400 at scale 1, 60 Hz virtual output, one
120 Hz producer, no input, no server decorations, five-second warmup and
twenty-second measurement. CPU includes process user and system time across
threads, excluding the producer and post-run capture.

| Presenter | CPU (% of one core) | CPU ms/composition | Compositions/s | Commits/s |
| --- | ---: | ---: | ---: | ---: |
| Direct wgpu surface | 4.55 | 0.760 | 59.85 | 120.05 |
| Minimal Weld–Bevy | 11.95 | 1.997 | 59.84 | 119.99 |
| Master | 12.25 | 2.047 | 59.84 | 120.02 |

All buffers were DMA-BUF; maximum observed GPU submissions in flight was one.
The roughly 1,200 superseded commits per case are expected when consuming a
120 Hz producer at 60 Hz. Captures confirmed the animated surfaces rendered.

The shared Bevy integration adds about 7.4 percentage points in this run; the
Master layer adds about 0.3. These are workload deltas, not a proof of unavoidable
engine overhead. Minimal still runs Weld's base surface, UI, input, extraction
and rendering integration. Main/render wall-time measurements cannot attribute
CPU costs to individual systems.

## Variation and controls

The matching 60 Hz producer control (`paced-render-np4u4707`) measured Master at
13.50%, with only four replacements over twenty seconds. Other runs varied:
the 1080p comparison (`paced-render-in2sk4wu`) measured 4.55%, 20.25%, and 14.75%
for surface, minimal, and Master. CPU placement/frequency were not controlled.
The minimal outlier means this benchmark does not consistently reproduce the
original Steam 20–30% workload. Repeat matched runs before claiming a saving.

Functional stress cases covered up to eight windows, 240 Hz per-window producer
cadence and 1,000 Hz pointer motion. Output remained near 60 compositions/s;
input reached the producer, captures contained all windows, and no growing GPU
queue appeared. The final input smoke run overlapped compilation and is excluded
from performance conclusions.

Next investigate the shared main-schedule, extraction and render-preparation
path with optimized symbolized CPU profiles. Keep the raw presenter as a
control. Separately validate changes against real nested/DRM presentation and
Steam; the fixture has simple opaque surfaces and does not exercise XWayland,
fractional scaling or physical scanout.

## Shared Bevy path: CPU sampling and named-system trace

The follow-up uses the optimized, symbolized `perf` profile with frame pointers.
`paced-profile-fhtqjqqd` samples only the minimal presenter process after warmup,
at 997 Hz for eighteen seconds: 1,027 userspace CPU-clock samples, zero lost.
The separate producer remains at 120 Hz and composition at approximately 60 Hz.

Exclusive branch classification of the sampled stacks gives:

| Branch | Share of sampled userspace CPU |
| --- | ---: |
| Other rendering preparation, including DMA-BUF preparation | 27.36% |
| Main schedule and admitted ingress | 24.34% |
| Render graph and GPU submission | 18.40% |
| Calloop dispatch | 8.86% |
| Extraction | 7.69% |
| GPU completion worker | 6.91% |
| Other host work / unattributed | 6.43% |

These are shares of userspace CPU, not percentages of a core or blocked GPU
wall time. The main branch includes query-archetype checks, system dispatch,
parameter access and deferred-command handling. No single copy or allocator
routine dominates. The measured process total in this run was 7.21%, lower than
the initial baseline; no production optimization had changed. The raw control
(`paced-profile-jw3zqtog`) overlapped offline symbolization, so its 3.13% total
is not a clean paired speedup comparison. Prefer repeated, uninstrumented
comparisons for claims about savings.

Following [Bevy's profiling guide](https://github.com/bevyengine/bevy/blob/main/docs/profiling.md),
the benchmark now initializes Weld's existing subscriber. `profiling-tracy`
selects Tracy; Bevy's `trace_tracy` feature already enables debug names.
Ordinary benchmark runs use warning-level logs. The normal compositor assembly
is unchanged.

`paced-tracy-wd4k1jm8/capture.tracy` contains a named-system trace. Aggregating
only events fully within trace seconds 6–18 gives 719 compositions and:

- 363 distinct system spans, including schedule-driver wrappers; about 367
  executions per composition. Many are cheap or early-returning. Counts alone
  establish neither CPU cost nor an optimization opportunity.
- 8 bind-group creations, 7 command-encoder finishes and 4 queue submissions
  per composition. These are actual resource/command operations in this
  instrumented build. `RenderDiagnosticsPlugin` is enabled by `tracing-tracy`
  and adds diagnostic resolve/readback work; the encoder/submission counts
  must not all be attributed to the ordinary build.
- No observed steady-state `Device::create_buffer` or
  `Device::create_texture` zones. The client image path reuses imported images.
- Sprite/mesh-2D/tilemap, gizmo and animation systems run in this UI-only fixture.
  Several preparation functions build view bindings even without corresponding
  geometry. `renderer::render_system` also submits an encoder after empty
  screenshot/readback helpers.

The feature graph explains part of the broad assembly: `ui_bevy_render` enables
`ui_api`, whose `common_api` bundle enables animation and gizmos;
`bevy_ui_render` also enables sprite rendering. `configure_rendering` installs
the resulting `DefaultPlugins`. Source inspection, rather than just the system
count, identifies these boundaries for narrower assembly experiments.

Tracy adds substantial overhead: this capture's process total was 21.24%.
Its per-zone times are instrumented wall times and must not be treated as
production CPU costs. Use the trace for identities, ordering and call counts;
use non-Tracy CPU sampling and repeated totals for cost. The full event CSV hit
the 128 MiB file bound and was discarded; the valid 8.4 MiB trace and streamed
steady-state aggregate JSON remain available without the large text export.

### Next experiments

1. A/B a narrower plugin assembly, preserving UI/text/picking and required
   asset/scene lifecycle while excluding unused rendering families. Measure
   total optimized CPU and verify pixels/input; early returns may already make
   some families too cheap to matter.
2. Avoid empty screenshot/readback submission, with regressions for real
   screenshot and readback requests. This also reduces encoder retirement work.
3. Cache stable view bindings and gate preparation on relevant content, with
   explicit invalidation for resize, target replacement and GPU recovery.

No CPU saving is claimed for these unimplemented candidates. Retaining UI
geometry across content-only frames is a broader follow-up if the simpler
experiments leave substantial extraction/preparation cost.

## Layer isolation: optimized, uninstrumented runs

The expanded benchmark isolates thirteen assemblies, from a direct surface
draw through the full Master distribution. Every case uses the real Wayland
producer and shared core buffer manager. The AppShell cases also retain the
production import, promotion and retirement path. Only the diagnostic draw
and selected schedule stages change. All additional hooks are behind
`test-support`; production rendering behavior is unchanged.

`paced-render-51og_doe` ran the matrix twice, reversing case order on the
second pass: release optimization level 3, Vulkan validation disabled,
2240×1400, one client-side-decorated window, 120 Hz producer, 60 Hz virtual
output, no injected input, three seconds warmup and eight seconds measurement.
No build or other profiling job ran concurrently. All cases sustained roughly
60 compositions/s and 120 commits/s, with at most one GPU submission in flight.

The warning-level subscriber exposed incomplete plugin initialization in the
two renderer-free main controls. Those two results are discarded. After
disabling their renderer-dependent plugin families and registering the shader
loader, clean reruns under the same settings are `paced-render-6uiloyxq`
(`framework-main`) and `paced-render-ny9udsvi` (`default-main`). The remaining
eleven matrix cases had no logged errors and use unchanged assembly paths.

| Case | Userspace instructions, million/s, two-run range | CPU, % of one core, two-run range |
| --- | ---: | ---: |
| Direct surface (`surface`) | 16.34–16.52 | 2.37–4.62 |
| Also empty Bevy App (`empty-app`) | 17.22–17.34 | 2.88–3.50 |
| Also MinimalPlugins (`core-app`) | 19.50–19.64 | 3.25–4.00 |
| Main framework, no renderer (`framework-main`) | 25.96–26.09 | 3.50–4.25 |
| Also Weld model/camera, no clients (`default-main`) | 26.39–26.44 | 4.25–4.63 |
| Real AppShell, main-only, no clients (`renderer-main`) | 26.97–27.29 | 5.00–6.25 |
| Real surface state and promoted images (`state-direct`) | 28.81–28.95 | 4.12–5.50 |
| State plus mounted SurfaceNode UI (`ui-direct`) | 30.14–30.38 | 4.38–4.75 |
| State plus extraction/deferred commands (`extract-direct`) | 33.73–33.81 | 5.63–5.75 |
| State plus render schedule, camera graph disabled (`prepare-direct`) | 46.80–46.97 | 6.24–7.62 |
| State plus full empty-camera render (`render-direct`) | 56.33–56.40 | 7.75–7.88 |
| Normal AppShell/SurfaceNode presenter (`minimal`) | 56.52–56.57 | 8.50–9.75 |
| Full Master fixture (`master`) | 65.25–65.27 | 8.50–11.13 |

The CPU columns include all process threads and kernel time. Instructions count
all benchmark threads in userspace, excluding the separate client producer.
Perf attaches after the measurement-start marker for an interior six-second
window; `counter-window.json` records counts and elapsed time, including small
attachment/exit overhead. All counters had full coverage. Instruction rates
are approximate but repeat closely. CPU placement/frequency remain uncontrolled;
CPU percentages vary too much to subtract neighboring rows as a reliable
percentage-point saving. Instructions are work counts, not a substitute for
CPU time or proof that each instruction costs the same.

### What this narrows down

- An empty Bevy App adds little. Main-world framework work accounts for about
  10 million additional instructions/s over direct rendering, before any
  client is represented in Bevy.
- Adding real Weld surface state costs about 1.7 million instructions/s over
  the initialized AppShell main-only control. Mounted UI adds about another
  1.4 million. This fixture does not show a dominant hidden Weld buffer-bridge
  or input cost.
- With no mounted client UI, extraction and temporary-entity retirement add
  about 4.9 million instructions/s over `state-direct`. The rest of the render
  schedule with camera rendering disabled adds about 13.1 million; enabling
  camera rendering adds about 9.5 million more. That is the largest remaining
  boundary to subdivide: preparation, render-resource/command lifecycle and
  camera work, rather than just the main ECS schedule.
- The full empty-camera renderer followed by a direct client draw performs
  almost as many CPU instructions as the normal client-UI renderer. Much of
  the cost therefore exists without extracting/drawing client UI geometry.
  Master adds measurable work too, but less than the renderer path.

These are controlled boundary comparisons, not a strictly additive stack:
`ui-direct` and `extract-direct` branch independently from `state-direct`.
`extract-direct` retires temporary render entities but skips preparation;
`prepare-direct` includes the normal cleanup and screenshot/readback tail.
`render-direct` also performs an empty Bevy clear/blit before the direct draw,
so its GPU command stream differs from `minimal`. Removing a schedule stage is
an experiment, not a production-safe optimization by itself.

The next useful experiments target render preparation and per-frame resource
creation/submission. Narrow plugin assembly and skipping empty readback work
remain candidates, not measured savings. Real Steam/DRM verification is still
needed: this fixture does not reproduce every part of its 20–30% workload.

### Verification

The release GPU matrix completed twice. The corrected renderer-free controls
also completed twice without warnings/errors. The separate `ui-direct` smoke
run `paced-render-_pwpphg8` displayed three distinct surfaces at 60 compositions/s
while accepting 360 commits/s and injecting 1,000 pointer motions/s. The client
reported receiving input; the captured image contained all three surfaces.

A focused regression covers promoted-image selection, pending replacement,
stable root ordering, unmap and destruction. Clippy for the benchmark with
`test-support`, the focused `weld-app` unit test and the ordinary distribution
check passed. The direct-draw probe is restricted to the fixture's upright,
opaque, uncropped roots. Live GPU binding retirement during window unmap is
not yet exercised by the producer. The older `shell_main` and `input_pipeline`
benchmarks were not rerun after correcting their shared renderer-free assembly;
their historical timing results should not be compared directly to new runs.
