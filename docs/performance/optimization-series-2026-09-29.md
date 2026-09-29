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

## Remaining priorities

1. Earlier source-side snapshot coalescing or incremental state publication,
   with explicit per-consumer readiness and preserved protocol/input semantics.
2. Avoid empty or unchanged Bevy schedule/render work.
3. Make per-wake host maintenance demand-driven.
4. Skip the empty screenshot/readback encoder submission.

Record each subsequent coherent slice separately, using the same optimized
workload and fresh comparison captures. Keep the timing effect separate from
correctness improvements and from unimplemented architectural possibilities.
