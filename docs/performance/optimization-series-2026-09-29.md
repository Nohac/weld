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

## Remaining priorities

1. Earlier source-side snapshot coalescing or incremental state publication,
   with explicit per-consumer readiness and preserved protocol/input semantics.
2. Avoid empty or unchanged Bevy schedule/render work.
3. Make per-wake host maintenance demand-driven.
4. Skip the empty screenshot/readback encoder submission.

Record each subsequent coherent slice separately, using the same optimized
workload and fresh comparison captures. Keep the timing effect separate from
correctness improvements and from unimplemented architectural possibilities.
