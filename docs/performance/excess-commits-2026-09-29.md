# Excess client commits — 2026-09-29

Production baseline: `f295008d`. No compositor behavior change is landed by
this investigation. The profiling runner gains hardware counters, instruction
sampling, and executing-CPU IDs.

## What the comparison establishes

Unpaced Blender costs more compositor CPU, but the entire difference cannot be
assigned to executing additional pre-Bevy instructions. Both cases run about
60 main schedules and compositions per second. Most **additional instructions**
are in host dispatch/adapter work; the existing main/render work also takes more
CPU time under the unrestricted workload. A total CPU flamegraph alone misses
this distinction.

The extra work is real and worth reducing. A fixed 5–6 percentage-point saving
from cheaper discarded commits is not established by these measurements.

## Conditions and evidence

Fully optimized `perf` profile, no Tracy, validation disabled, factory Blender,
middle-button orbit at approximately 997 input events/s, 1815×1179 viewport.
No builds or other GPU tests run during captures. Paced means Blender alone
receives Mesa `vblank_mode=3`; Weld's production pacing is unchanged. The host is
SwayFX 0.6, based on Sway 1.12. CPU percentages are one-core equivalents, summing
Weld's user and system CPU and excluding Blender.

All artifacts below are in `target/validation/`:

| Run | Client | Seconds | Measurement | CPU |
| --- | --- | ---: | --- | ---: |
| `orbit-hoag9o25` | Unpaced | 30 | CPU stacks, original binary | 20.57% |
| `orbit-tf9pm4a5` | Paced | 30 | CPU stacks, original binary | 11.23% |
| `orbit-u6p8cktu` | Unpaced | 30 | CPU stacks + counters | 16.47% |
| `orbit-d84y00hu` | Paced | 30 | CPU stacks + counters | 12.67% |
| `orbit-2dikl4vl` | Paced | 30 | Instruction stacks + counters | 12.00% |
| `orbit-tvvaelhd` | Unpaced | 30 | Instruction stacks + counters | 17.80% |
| `orbit-uy70jzfn` | Unpaced | 25 | CPU stacks + counters + CPU IDs | 16.56% |
| `orbit-2otpsny4` | Paced | 25 | CPU stacks + counters + CPU IDs | 11.96% |

After the first pair, a temporary probe counts outer-loop iterations, native
main advances, requested compositions, and events popped from the runtime
queue. It emits one line per second. This is an optimized diagnostic binary,
not an uninstrumented production measurement. The probe was removed afterwards;
its [diagnostic-only patch](fixtures/excess-commit-counters.patch) applies to
`f295008d` for reproduction, and its binary is retained as
`target/perf/weldwm-orbit-commit-counters`. A same-basename symbol copy is in
`target/validation/commit-probe-symbols/`; use
`perf script --no-inline --symfs target/validation/commit-probe-symbols,flat`
for these diagnostic captures after the ordinary optimized binary is rebuilt.
Existing Tracy zones also provide
these boundaries, with substantially higher instrumentation overhead.
The recorded `settings.json` identifies the sampled event for each run.
The captures include an additional context-switch counter that produced zero
with userspace-only restrictions; the final runner omits that unhelpful counter.

Excluding the first and last diagnostic intervals within the capture:

| Rate | Unpaced, `u6p8cktu` | Paced, `d84y00hu` |
| --- | ---: | ---: |
| Runtime surface events/s | 284.12 | 60.00 |
| Host iterations/s | 2129.87 | 1678.56 |
| Main advances/s | 59.98 | 59.96 |
| Compositions requested/s | 59.98 | 59.96 |

Runtime events have already passed the runtime queue's coalescing; they are not
raw Wayland commit counts. Later pairs reproduce roughly 295 versus 60 events/s
and 60 compositions/s. This agrees with the earlier independent Tracy counts
in [render costs](render-costs-2026-09-29.md#repetition-and-cadence).

### More instructions versus slower execution

The instruction-sampled pair retired 4.189 billion instructions unpaced versus
3.651 billion paced: **14.7% more**. Userspace cycles increased from 8.103 to
11.420 billion: **40.9% more**. Instructions/cycle fell from about 0.451 to
0.367. Hardware counters reported 100% running time. Recorded-period-weighted
samples cover approximately 98.7–98.9% of the corresponding instruction totals.

Disjoint stack classification, in millions of sampled instructions over 30s:

| Branch | Paced | Unpaced |
| --- | ---: | ---: |
| Main application advance | 616.4 | 600.2 |
| AppShell rendering | 1258.5 | 1318.5 |
| Calloop dispatch | 132.3 | 481.0 |
| Client adapter servicing | 171.3 | 233.6 |
| Prepare-dispatch / input | 662.1 | 671.8 |
| Native presentation outside AppShell rendering | 438.8 | 498.1 |
| Completion worker | 59.5 | 72.3 |
| Other | 263.7 | 265.4 |

Classify stacks once, with AppShell rendering/main taking precedence over
their enclosing native branches. Do not add inclusive callees to this table.
Sampling error and compiler inlining limit individual-function attribution.
Dispatch and adapters account for about three quarters of the sampled
instruction increase. Main/render instructions remain broadly similar.

The final CPU-sampled pair puts main + AppShell rendering at approximately
7.7 versus 5.8 CPU percentage points, despite their matching frame cadence.
These are userspace sample proportions scaled by the process user-time counter,
not direct per-function CPU clocks. The samples cannot explain kernel time.

GPU utilization averaged 82–84% unpaced versus about 26% paced. CPU-core
sampling found 73.5% versus 78.0% of main-thread time samples on CPUs 0–7,
whose reported maximum frequency is higher than CPUs 8–19. Core migration alone
is not established as the explanation. The measured execution-efficiency loss
is real; separating cache/memory contention, CPU placement, and power/frequency
effects requires a controlled follow-up. Do not call it a proven GPU-bandwidth
bottleneck or attribute all extra CPU to discarded-frame bookkeeping.

### Protocol control

`orbit-808j4if5` enables client Wayland logging for 12 seconds. It records 4209
surface commits/attaches and buffer releases, only one `create_immed`, and no
explicit-sync acquire/release-point requests during the capture. The syncobj
global is advertised, but this Blender uses implicit synchronization. Its timing
is excluded from performance comparisons because protocol logging perturbs it.

The capture supports reusable DMA-BUF imports, not per-frame Vulkan texture
creation. `DmabufSourceCache` imports when the protocol buffer is created;
per-commit `surface_tree::import_buffer` mostly obtains metadata and a lease.
SHM copying is a different path and is not measured by this Blender test.

## Comparison with Sway/wlroots

Read upstream sources rather than inferring behavior from Sway's CPU total:

- [Sway output scheduling](https://github.com/swaywm/sway/blob/master/sway/desktop/output.c)
  schedules repaint/frame callbacks around output refresh and checks whether
  scene output work is needed.
- [wlroots 0.19 surface state](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/0.19/types/wlr_compositor.c)
  applies committed fields and buffer replacement; this is not wholesale
  rejection of surplus `wl_surface.commit` requests.
- [Scene surface updates](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/0.19/types/scene/surface.c)
  update buffer/geometry/damage and schedule an output frame. Output membership
  and preferred scale have their own output-update notifications.
- [Client buffers](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/0.19/types/buffer/client.c)
  attempt texture updates/reuse;
  [GLES imports](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/0.19/render/gles2/texture.c)
  reuse the buffer-associated EGL image/texture.
- [Explicit sync](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/0.19/types/wlr_linux_drm_syncobj_v1.c)
  checks fence availability before installing a waiter. The scene carries the
  acquire timeline to rendering. Weld's current explicit path installs a CPU
  readiness blocker and consumes the acquire point before snapshot publication.
  This difference does **not** explain the measured implicit-sync Blender run.

The wlroots 0.19 branch is a concrete architectural reference, not a verified
exact match to the installed SwayFX dependency or every current compositor.
The old GitHub wlroots mirror is archived and was not used for these conclusions.

## Actionable host-side work

1. **Make output propagation dirty-driven.** Every processed toplevel commit
   currently calls `apply_surface_tree_outputs`: allocate/collect the surface
   tree, revisit memberships, and traverse again for scale. Pixel-only commits
   should retain existing output state; topology/new-subsurface/scale changes
   still need immediate propagation.
2. **Avoid unrelated maintenance on each wake.** Winit pumping, keyboard
   settings polling, independent-presenter root enumeration, adapter servicing,
   client flushing, and child reaping share the outer loop. Both independent
   callback methods enumerate mapped roots even when there are no claims.
   Use readiness/demand information where possible without delaying input,
   buffer releases, or protocol replies.
3. **Separate retained source updates from snapshot publication.** Currently
   each processed commit traverses the tree and builds a snapshot, followed by
   adapter translation/leases, before consumer coalescing. A dirty retained-state
   boundary could materialize snapshots at consumer admission. Preserve mapping
   barriers, synchronized subsurfaces, input-region updates, damage, callbacks,
   and all buffer-lifetime rules. This needs a lifecycle slice, not a blind
   early-return/drop in the commit handler.

There is no measured saving from these proposed changes yet. The instruction
comparison supports investigating them before further small render-system
tweaks, but not promising that they recover the whole observed CPU gap.
