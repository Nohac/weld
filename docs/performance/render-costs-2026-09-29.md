# Post-fix compositor CPU costs — 2026-09-29

Measured production revision: `d54b1504`. This investigation changes profiling
tools, not compositor behavior. It follows the buffer-release and repaint fixes
recorded in [the orbit investigation](blender-orbit-2026-09-28.md).

## Findings

- Full-output nested Blender orbit costs about **16–17% of one CPU core** in
  the optimized, non-Tracy binary with Vulkan validation disabled.
- The same pointer workload with only Blender paced using Mesa's
  `vblank_mode=3` costs **9.8%**. A separate trace confirms both Blender commits
  and Weld compositions continue at **60/s**. Excess client work is material;
  the rendering framework does not impose a demonstrated 16% CPU floor.
- Direct Sway costs **3.6%** in the corresponding control. It also accepts an
  unpaced Blender, so pacing alone does not explain away Weld's overhead.
- The untraced CPU profile attributes about **45% of userspace samples** to
  composition/presentation and **26%** to the main application schedule.
  The remaining work is distributed across input, Wayland servicing, and
  supporting threads. No single allocation/copy function dominates.
- Every composition currently makes **five wgpu submissions**, plus a sixth
  Vulkan submission inside presentation. One wgpu submission is an empty
  Bevy screenshot/readback encoder in this workload. Nine bind groups are
  created per composition: eight in Bevy systems and one for Weld's final blit.

These are measurements and optimization candidates, not measured savings from
unimplemented changes. No production optimization was landed by this pass.

## Conditions and CPU totals

Ryzen AI 9 365, Radeon 880M/RADV STRIX1, Mesa 26.2.3, 2240×1400 output at
60.002 Hz, Sway scale 1.25. Factory Blender, middle button held, approximately
998 relative pointer events/s, fullscreen temporary workspace. No compilation
ran during a measurement interval. CPU is `(utime + stime) / elapsed`, summed
over Weld threads, **excluding Blender**. Direct-Sway rows measure Sway itself.

All runs below disable the Khronos validation layer. Artifacts are under
`target/validation/`; they are local, untracked evidence.

| Run | Configuration | Seconds | CPU, one core |
| --- | --- | ---: | ---: |
| `orbit-ls4rl78b` | Development, no profiler | 12 | 20.75% |
| `orbit-pqzj3k0s` | Development, no profiler | 12 | 17.75% |
| `orbit-_8maljt4` | Optimized, no profiler | 12 | 16.58% |
| `orbit-n7e79hwm` | Optimized, perf/frame pointers | 30 | 16.13% |
| `orbit-8auvdiec` | Optimized, perf/DWARF 16 KiB | 20 | 16.80% |
| `orbit-wnecswtj` | Optimized, Tracy + perf | 20 | 31.75% |
| `orbit-0zox29tg` | Direct Sway, no profiler | 12 | 3.58% |
| `orbit-uh9ag6ob` | Optimized, paced Blender, no profiler | 12 | 9.75% |

The optimized build uses the `perf` Cargo profile with frame pointers and no
`profiling-tracy` feature. The traced variant has that feature enabled. Stable
binary snapshots preserve symbols when Cargo changes feature sets.

These short runs establish useful magnitudes, not confidence intervals. Sway
and nested Weld fill the same output, but their decorations/layout produce
slightly different Blender viewports: 1837×1238 versus 1815×1178. The direct
comparison is therefore a practical workload comparison, not identical pixels.
The manual DRM control is recorded separately below; nested timings cannot
establish the cost of direct scanout or the DRM completion wait.

### Manual DRM control

The user ran `scripts/profiling/drm-orbit`, manually orbiting Blender:
`target/perf-traces/blender-orbit-drm-drm-20260929-160910.*`.
The metadata confirms the same prepared non-Tracy optimized binary.

- 20-second capture, 2.76 user + 0.53 system seconds: **16.45% of one core**.
- 1,341 userspace samples, zero lost samples; about 1,377 expected from the
  process user counter and sampling frequency.
- DRM output 2240×1400 at 60 Hz; Blender viewport 1819×1202.
- The CPU accounting check passed; normal DRM device teardown is logged.

This is consistent with nested's broad CPU level, but manual motion and
slightly different viewport geometry prevent a strict backend-only A/B.
The 16 KiB DWARF stacks reach `NativeRuntime::run` for only 52% of sample
weight, so their top-level inclusive percentages underestimate deep paths.
Leaf attribution still shows about 25% in Bevy ECS symbols. Do not interpret
the difference from nested's inclusive percentages as a measured DRM saving.

The generic runner's inline-expanded post-processing did not finish producing
its final report/quality files. The original data and CPU counters are intact;
the cause of that post-processing interruption was not established. A separate
fast rendering of the saved data is available as `.noinline.folded` and
`.noinline.svg`. The zero-loss and CPU-accounting statements above were checked
directly rather than inferred from a nonexistent final success file.

### Profiler overhead

The traced run consumed 6.35 CPU seconds in 20 seconds. Its Tracy Profiler
thread alone consumed 0.71 seconds; tracing span bookkeeping also runs on the
main thread. Its hottest sampled leaf was Tracy compression. Do not use that
run's 31.75% CPU as the production baseline.

For the non-Tracy 30-second sample:

- Process: 3.83 user + 1.01 system seconds.
- Main thread: 3.53 user + 0.85 system seconds.
- DMA-BUF completion thread: 0.15 user + 0.04 system seconds.
- Winit event monitor: 0.10 user + 0.15 system seconds.

Thread-counter rounding and thread lifetimes account for small sum differences.
Both perf captures report zero lost samples. Sampling uses `cpu-clock:u`, so
the following attribution **excludes kernel CPU**, even though the total CPU
figures above include it. GPU waits and blocked wall time are not CPU costs.

## Untraced CPU attribution

`orbit-n7e79hwm/cpu.svg` is the flamegraph; `cpu.folded` contains period-weighted
stacks. Each stack is assigned once to a top-level branch below. Nested costs
such as DMA-BUF preparation are already included in composition.

Stack processing used `perf script --no-inline`, piped into
`stackcollapse-perf.pl` and `flamegraph.pl`. Generic/monomorphized symbols can
hide system names; the table describes host entry-point subtrees, not precise
per-system attribution. Inspect inline-expanded stacks before making claims
about a particular unnamed Bevy system.

| Branch | Userspace sample share |
| --- | ---: |
| Nested presentation, including AppShell rendering | 44.70% |
| AppShell main application schedule | 26.04% |
| Nested prepare-dispatch, including Winit/input | 8.43% |
| Main calloop dispatch | 6.96% |
| DMA-BUF completion worker | 3.95% |
| Client adapter servicing | 2.79% |
| Other or unattributed | 7.12% |

Within those branches, AppShell `render_outputs` is 38.59% of all userspace
samples, the final nested renderer is 5.17%, and DMA-BUF `prepare_render` is
4.27%. These overlap the branch totals and must not be added to them.
About 2.4% of all userspace samples are deferred client commit application
through `blocker_cleared`, within calloop dispatch. That is real commit work,
not purely polling/wakeup overhead.

The main schedule's hottest leaf groups are query archetype checks, system
dispatch/type/parameter access, and the single-threaded executor. Across the
whole process, leaf symbols containing `bevy_ecs::` account for about 28% of
samples; the DWARF control gives about 27%. This is framework bookkeeping
spread over many systems, not evidence that tiling or one UI layout function
consumes 28%. UI layout alone is below 1% of userspace samples here.

The 16 KiB DWARF capture agrees on the broad workload but loses outer callers
more often in deep render stacks. Use the frame-pointer capture for the branch
table, and keep the DWARF result as a cross-check rather than averaging the two.

## Repetition and cadence

Tracy counts use central active windows, excluding startup and capture tails:
10–26 trace seconds for `orbit-wnecswtj`, and 10–18 for the paced control
`orbit-1f_cowlu`. These instrumented runs establish call counts; their wall
durations are perturbed and include waits.

| Event | Unpaced, per second | Paced, per second |
| --- | ---: | ---: |
| Composition | 59.94 | 60.00 |
| Surface-tree update | 336.81 | 60.00 |
| Calloop iterations / Winit pumps | 1913.50 | 1560.12 |
| Input ingress batches | 732.12 | 846.62 |
| wgpu queue submissions | 299.69 | 300.00 |
| Vulkan queue submissions | 359.62 | 360.00 |

Blender's draw counter independently shows roughly 330–360 draws/s unpaced
and 60/s paced, at the same nested viewport size. The probe's modal input
counter does not count orbit motion reliably because Blender's orbit operator
consumes it; use the helper's sent events and actual draws instead.

Pacing eliminates repeated client commits while retaining 60 compositions/s.
It also changes Blender's GPU load and scheduling, so the 6.8 percentage-point
CPU difference must not be attributed exclusively to one server function.
`vblank_mode=3` is a diagnostic client override, not a proposed global policy.
Reducing input delivery rate, delaying buffer release, or withholding callbacks
can harm latency or reintroduce the previous fixed-buffer-pool starvation bug.

### Submission and resource paths

The five wgpu submissions, in order:

1. DMA-BUF acquisition barriers (`dmabuf/manager.rs::prepare_render`).
2. Bevy render graph, with four command buffers in this trace.
3. Bevy's screenshot/readback encoder (`bevy_render::renderer::render_system`).
4. DMA-BUF retirement barriers (`dmabuf/manager.rs::finish_render`).
5. Weld's nested composition blit (`renderer/mod.rs::NestedRenderer::render`).

The sixth Vulkan submit is nested under
`Surface::present > Queue::present`, confirmed by trace ancestry. It is not a
sixth explicit Weld `Queue::submit` call.

The eight per-frame Bevy bind-group creation sites are mesh2d views, sprite
views, UI nodes, UI slices, UI gradients, box shadows, UI materials, and OIT
resolve. The ninth is Weld's composition blitter. Steady-state Weld surface
material binding preparation does not recreate bindings every frame: the
unpaced window has zero such creations, the paced window has one total.

Relevant source:

- `crates/weld-core/src/dmabuf/manager.rs` — acquisition/retirement submissions.
- `vendor/bevy-wgpu30/bevy_render/src/renderer/mod.rs` — unconditional
  screenshot/readback encoder and submission, even when both helpers record
  no commands.
- `crates/weld-core/src/renderer/mod.rs` — nested bind group, blit, submit/present.
- `crates/weld-core/src/backend/nested.rs::prepare_dispatch` — Winit pumping
  and scale-factor query on every main-loop iteration.
- `crates/weld-core/src/server/presentation.rs` — both independent callback
  methods enumerate mapped roots even when none have a remote claim.
- `crates/weld-core/src/runtime/native.rs` — per-iteration child reaping.
- `crates/weld-core/src/backend/drm/renderer.rs` — synchronous GPU completion
  in `submit_pending` / `submit_and_wait`; wall-time/latency concern, not proven
  CPU consumption.

## Isolated rendering control

Existing `shell_render` benchmark, optimized without Tracy, validation off,
300 measured frames after 30 warmup frames per case. The reported adapter is
Radeon 880M Vulkan, not a software renderer. Output:
`target/validation/render-costs-2026-09-29-bench.log`.

For a mapped retained synthetic client with 16 motion events/frame:

- Input and commit ingress: 4.0 µs/frame combined.
- Main update: 192.9 µs/frame.
- Render submission: 426.6 µs/frame.
- Explicit GPU completion wait: 767.3 µs/frame.

Other cases put main updates at 81–177 µs and render submission at 199–401 µs.
These are **wall times**, not sampled CPU. The benchmark uses a 1920×1080
offscreen output, a 900×600 synthetic SHM client, Float policy, and an explicit
per-frame GPU wait. It excludes Winit, real client DMA-BUF churn, and Master
tiling; it is an isolation control, not a prediction of desktop utilization.

## Recommended implementation order

1. **Skip empty screenshot/readback encoder creation and submission.** The
   source and trace agree on one unnecessary submission/frame. Preserve real
   screenshot/readback behavior and verify resource maintenance still runs.
   A/B total CPU, submission count, screenshots, and surface teardown. All
   submissions combined are only about 5% of userspace samples in this run;
   this small fix alone cannot explain or remove the entire gap to Sway.
2. **Make idle host servicing demand-driven.** Avoid empty-claim root walks;
   reap on child-exit readiness; pump Winit on its actual readiness/deadline
   while preserving cursor flushing and input responsiveness. Measure syscall
   counts and system CPU as well as user stacks. The root walks alone are
   small at one window; scaling and wake frequency are the motivation.
3. **Reduce framework work per composed frame.** Audit installed rendering
   systems for empty workloads and cache stable view/bind-group state. The
   repeated ECS bookkeeping and nine bind-group creations are measured, but
   removing individual optional systems requires dependency/lifecycle checks.
   Compare the isolated benchmark and the real orbit workload.
4. **Consolidate rendering submissions/targets.** Integrate acquisition,
   rendering and retirement ordering without weakening DMA-BUF ownership;
   use external composition targets to evaluate removing the final nested
   blit. This is a higher-risk lifecycle change, not a quick submission delete.
5. **Handle overproducing clients cheaply.** Trace same-thread lease-drop
   notifications and commit coalescing before changing them. Batching host
   bookkeeping is preferable to accidentally starving clients. A paced-client
   option needs an explicit latency/policy decision.

   DMA-BUF readiness blockers are another measured servicing path (about 2.5%
   of unpaced userspace samples, mostly deferred commit application rather than
   blocker overhead). A not-yet-ready commit can install a calloop
   source, later wake dispatch, then remove the source. Count successful
   installs before claiming this happens on every commit: Smithay's
   `DmabufSource::new` already checks fences and returns `AlreadyReady` when
   appropriate. Adding another equivalent readiness check is not a new fix.

Investigate the DRM blocking completion path using the manual profile before
proposing an asynchronous scanout handoff. Direct-scanout/damage optimizations
also need DRM evidence; the nested test cannot validate them.

## Review and verification

An independent Fable source audit ran without subagents alongside profiling.
Its submission and per-wake findings were checked against source and captures.
Fable also approved the diagnostic tooling after its final review. Eleven
Python tests and Bash syntax checks passed; both nested and manual DRM runners
produced live captures.
One plausible-looking hypothesis was rejected: toggling `Camera::is_active`
does not by itself force UI relayout in the pinned implementation. The camera
recompute checks `is_added`, projection and viewport changes, and downstream UI
propagation compares values before updating derived state.

The existing Vulkan acquire-fence validation warning remains outside this
pass. Forced test shutdown can produce Blender EGL/epoxy errors after the
capture boundary; CPU results cover the preceding live interval. No GPU reset
or in-capture process failure was observed in these measurements.
