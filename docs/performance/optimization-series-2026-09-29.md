# Optimized compositor improvements — 2026-09-29

Tracking implementation against the [measured costs](render-costs-2026-09-29.md).
Headline comparisons use the fully optimized `perf` profile **without Tracy**,
Vulkan validation disabled, factory Blender orbit at approximately 1,000 Hz
synthetic pointer input, and the same fullscreen nested output. Client vsync is
not overridden. Each capture lasts 20 seconds after four seconds of warmup;
no compilation or other GPU test runs during capture. CPU is percent of one
core, including user and system time, excluding Blender.

## 1a. Coalesce before local presentation preparation

Checkpoint: `bfa978de`. This first part moves coalescing ahead of Bevy image and
snapshot preparation. It does not yet defer the Smithay surface-tree snapshot
or generic client-event construction; those remain possible follow-up work.

The local presenter keeps the latest unpublished commit for each surface,
including interleaved multi-window updates. It preserves retained layers and
ordered control/map transitions. Other consumers retain their independent
cadence. Superseded leases release without reaching Bevy preparation, while
the latest lease pins its imported texture even if the protocol buffer is
destroyed before preparation. Import namespace checks preserve renderer
ownership across that deferred interval.

Artifacts are under `target/validation/`:

| State | Run | Sampling | CPU | Blender viewport draws/s |
| --- | --- | --- | ---: | ---: |
| Checkpoint | `orbit-yc4s4ft8` | None | 15.10% | 347.9 |
| Presenter inbox | `orbit-7q3f7226` | perf, frame pointers | 15.70% | 341.3 |
| Presenter inbox | `orbit-d7lgvl6_` | None | 14.25% | 342.7 |
| Presenter inbox | `orbit-5pihig9j` | None | 14.70% | 343.5 |

All four viewports were 1815×1178. The sampled run lost zero samples. The two
uninstrumented repeats are 0.40–0.85 percentage points below the fresh baseline
(approximately 3–6% relative). This is a modest signal from short runs, not a
confidence interval or proof that the entire difference is due to coalescing.
The sampled result does not show a win. No part of the earlier 6–7 point
paced-client difference should be claimed as recovered by this slice.

Verification: 61 `weld-app` library tests, 132 `weld-core` library tests, and
the separately enabled Vulkan import-retirement lifetime test passed. The
AppShell regression checks that overwritten uses complete before the main
advance, while the surviving SHM use is prepared exactly once at that advance.
The inbox tests cover interleaved surfaces, retained content, and lifecycle
ordering. Existing multi-output rendering coverage still passes.

Fable approved the code review. Remaining coverage gaps: the import-retirement
test exercises the lease/cache boundary with an offscreen texture, not a real
client destroying a DMA-BUF `wl_buffer` before display. There is no new manual
DRM or network-hoist regression run for this slice. Clippy for the affected
crates with test support and warnings denied passed.

## 1b. Shared snapshots and consumer-owned admission

Checkpoint: `a0b548a`. Pending-commit policy now lives in `weld-client` and is
used by the runtime event queue, local presenter, and encoded source. Consumers
share reference-counted commit state; cloning a commit retains its geometry and
inventory vectors without copying them. Mutation is explicitly copy-on-write,
and an admitted consumer takes uniquely owned vectors directly. Retained-buffer
merging no longer builds a temporary hash map. Encoded alpha conversion happens
at wire admission, so queued commits retain the shared snapshot unchanged.

Bevy's second, prepared-event coalescer was removed. It could otherwise collapse
the map/unmap transitions preserved by the earlier neutral queue. Prepared
events now apply in admission order; the pre-role image cache remains local to
presentation and carries prepared content without temporary lookup collections.

All captures below used the same 1815×1179 Blender viewport, optimized profile,
validation disabled, no profiler, and no concurrent compilation or GPU tests:

| State | Run | CPU | Blender viewport draws/s |
| --- | --- | ---: | ---: |
| Checkpoint | `orbit-b0_nbtbe` | 14.25% | 326.9 |
| Shared snapshots | `orbit-ma42bh3u` | 14.20% | 335.0 |
| Shared snapshots | `orbit-0l_tiwrh` | 14.45% | 336.9 |

This is **CPU-neutral within the observed short-run variation**, not a measured
speedup. The snapshot-sharing and unique-consumption tests establish eliminated
inventory copies; they do not establish how much total compositor CPU those
copies cost. The single-window workload also does not measure multi-consumer
hoist fan-out. Initial Smithay tree traversal, snapshot construction, and SHM
copying still happen per processed commit. Those costs remain candidates for
separate measurement and optimization.

Verification: 51 neutral-client tests, 44 relay/core tests, 138 encoded-port
tests, 59 application tests (including GPU presentation), and 132 compositor
core tests passed. The explicit Vulkan import-retirement lifetime test also
passed; the unrelated native keyboard socket test remains ignored. Regressions
cover independently paced consumers, copy-on-write isolation, retained content,
capacity replacement/rejection, session boundaries, ordered mapping changes,
and prepared-event ordering. The separate Godot receiver workspace passes
library/test compilation. Affected-crate clippy passes with warnings denied.
The real-transport suites pass as well: 65 Iroh tests and 24 Unix transport tests.
The `weld-ssd`, `weld-hoist`, `weld-window`, and `weld-window-ui` library suites
also pass (49 tests combined).

The broader `weld-float` suite passes 17 tests and fails
`ending_an_already_settled_resize_removes_the_session_and_anchor`. The identical
assertion at `crates/weld-float/src/lib.rs:2118` also fails in the untouched
`target/debug/deps/weld_float-f2a6881541b3dc5c` binary built on September 12,
before this slice. That fixture constructs Window/Float plugins directly,
without the surface ingress plugin, and never advances a client commit revision
to settle the resize request. This existing resize-test issue is left unchanged;
the broader suite is not reported as entirely green.

Fable approved the code review. There is no new manual DRM, network media, or
headset playback run for this slice; transport tests use fixture media. Buffer
import retirement is covered by the offscreen Vulkan fixture, rather than a
real client destroying its protocol buffer before display.

## 1c. Reuse source-tree bookkeeping and effective input regions

Checkpoint: `32a5ad80`. The source tree now reuses its committed-node Vec,
live-node HashSet, content-update HashMap, and retained-node Vec capacities.
Collected nodes move into retained state after applying their buffer assignments.
Scratch contents are drained/cleared before returning; only storage survives the
turn. Protocol dispatch, synchronized-tree application, explicit-sync release
ordering, initial SHM copying, and per-commit snapshot publication stay immediate.

Effective input rectangles are cached by the ordered region definition and
effective bounds. Pixel-only commits reuse them, and unchanged protocol regions
are not cloned. Region edits, crop/size changes, and unmap/remap invalidate the
cache. Explicit root input regions still use the full surface bounds rather than
the cropped window bounds.

A fresh optimized userspace sample (`orbit-affgq85i`, zero lost samples) put
`SurfaceTreeState::update` at approximately **1.33% inclusive / 0.37% self** of
sample weight. This is a short statistical sample, not an exact per-call timing.
The sampled total was 17.10% CPU with 351.3 Blender viewport draws/s. Bevy
schedule execution, including rendering schedules, remains much more material.

Uninstrumented captures, all 20 seconds, validation off, 1815×1179 viewport:

| State | Run | CPU | Blender viewport draws/s |
| --- | --- | ---: | ---: |
| Checkpoint | `orbit-66ux4i5p` | 14.45% | 333.3 |
| Source cache | `orbit-snrxba8g` | 15.50% | 331.3 |
| Source cache | `orbit-_hvkjcfr` | 16.35% | 350.4 |
| Checkpoint, rebuilt | `orbit-0jz13o1z` | 15.45% | 339.3 |
| Source cache, retained binary | `orbit-gfzyo0qm` | 13.80% | 335.2 |
| Checkpoint, retained binary | `orbit-4tk_yr1o` | 17.15% | 352.6 |

The last three runs alternate retained binaries without intervening compilation.
The initially higher candidate measurements also occur on the original code.
These results establish **no repeatable whole-compositor CPU improvement** and
do not establish a regression. The allocation and recomputation reductions are
structural; their timing effect is below what these variable short captures can
isolate. Further source snapshot deferral is not the next priority for this
workload, given the small sampled share. All captures exclude concurrent builds
and other GPU tests.

Verification: 135 core tests passed, including new input cache invalidation and
storage-reuse coverage; two hardware/socket tests remain ignored in this run.
Core all-target clippy passes with warnings denied. `scripts/check-host-runtime
--shm-only` passed with 61 frame callbacks and 61 buffer releases, unmap/remap,
client exit, host shutdown, and socket cleanup (`host-runtime-z7eez8h2`).
Fable approved the source change. No new multi-subsurface protocol fixture,
manual DRM run, or network-media run was added for this internal source change.

## 2a. Preserve stable Bevy view state and cache the UI-root query

Checkpoint: `02ca392f`. The fresh optimized sample `orbit-1kdyb5iz` attributes
39.12% of userspace sample weight to `AppShell::render_outputs` and 25.94% to
`advance_main`, with zero lost samples. Main/render scheduling remains a large
part of the cost, spread over many systems. The earlier Tracy capture
`orbit-wnecswtj` identifies the exclusive UI-root query setup among the recurring
systems; its instrumented durations are not headline CPU measurements.

This slice changes three specific behaviors:

- The UI rounding policy uses a cached Bevy `Query` and deferred commands,
  ordered before layout, instead of constructing `QueryState` every frame.
- Stable output views retain their Bevy wrapper/identity and change ticks;
  camera activity is written only on an actual change.
- The vendored camera system compares manual-target size and scale with its
  computed metrics. Texture rotation alone leaves projection/frustum state
  unchanged. New metrics, viewport/sub-camera changes and explicit projection
  changes still update it. The patch is recorded in the vendor refresh note.

All following measurements use the optimized non-Tracy binary, validation off,
25-second captures, and 1815×1179 Blender viewport. No build or other GPU test
runs during capture. The original binary was retained for alternating controls.

| State | Run | Client draws/s | CPU |
| --- | --- | ---: | ---: |
| Original, perf sample | `orbit-1kdyb5iz` | 337.5 | 14.52% |
| Original | `orbit-pp2pm_f7` | 344.0 | 15.00% |
| Stable views/query | `orbit-a41l_yno` | 347.9 | 17.00% |
| Original repeat | `orbit-ohpu3r9r` | 345.1 | 14.16% |
| Stable views/query plus experimental MSAA-off | `orbit-_ms34mvq` | 353.2 | 16.08% |
| Original, client-vsync control | `orbit-hftyvbrw` | 60.0 | 10.88% |
| Stable views/query plus MSAA-off, client-vsync control | `orbit-x0iikjmy` | 60.0 | 11.44% |
| Original, client-vsync repeat | `orbit-51bxb1_8` | 60.0 | 10.96% |
| Stable views/query, client-vsync control | `orbit-xt258010` | 60.0 | 10.56% |

The final subset shows a 0.32–0.40 percentage-point reduction in **one** fixed-
cadence control, not a demonstrated repeatable win. The unrestricted results do
not show improvement. Structural elimination of redundant work is verified by
change-tick tests; overall CPU benefit remains unproven. The client-vsync rows
set Mesa's `vblank_mode=3` only in the test client, not Weld's production pacing.

### MSAA experiment — default unchanged

The compositor camera inherits 4× MSAA, but Bevy's UI pass draws to the unsampled
attachment. The optional experiment set the compositor camera to `Msaa::Off`,
avoiding the unused multisampled color attachment and reducing depth samples.
A real GPU comparison of fractional-position rounded translucent UI and soft
shadows produced identical pixels. Nevertheless the experiment did not establish
a CPU benefit, so the production MSAA default is left unchanged. The independent
pixel comparison remains as regression coverage for future rendering work.

Verification: all 62 weld-app tests pass on Vulkan, including unchanged camera/
projection/resource ticks, independent size and scale changes, explicit
projection changes, UI-root reparenting and ordering, sequential two-output
rendering, and actual pixels on two rotated external targets. All-target clippy
with warnings denied and the workspace all-target check pass.

The native startup screenshot with foot timed out on both candidates
(`bevy-view-smoke-6WHsGB`, `bevy-view-smoke-rhkVqV`) and the preserved original
binary (`bevy-view-baseline-YlXOZV`). This is a separately reproduced capture/
readiness issue, not a passed smoke test or evidence of a new regression.
No manual DRM or network-media run was performed.

Fable approved the view/query changes and subsequently the GPU-test additions
and MSAA experiment. Final confirmation after restoring the original MSAA
default was quota-blocked; that final subset review is deferred, with no
unresolved findings from the completed reviews.

## Remaining priorities

1. Avoid empty or unchanged Bevy schedule/render work.
2. Make per-wake host maintenance demand-driven.
3. Skip the empty screenshot/readback encoder submission.
4. Revisit source snapshot deferral/incremental publication if multi-window or
   complex surface-tree profiles show material cost, preserving per-consumer
   readiness and protocol/input semantics.

Record each subsequent coherent slice separately, using the same optimized
workload and fresh comparison captures. Keep the timing effect separate from
correctness improvements and from unimplemented architectural possibilities.
