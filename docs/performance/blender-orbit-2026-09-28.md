# Nested Blender orbit CPU investigation

Initial investigation against `c2416036` on 2026-09-28. Production code was
unchanged for the baseline below. Findings and the diagnostic launcher under
`scripts/profiling/` were checkpointed in `f57a2ef`; the implementation
follow-up is recorded at the end.

## Reproduction and controls

Hardware: Ryzen AI 9 365 / Radeon 880M, Mesa 26.2.3, 2240x1400 display at
60.002 Hz, Sway scale 1.25. Blender uses factory settings and the default cube.
Each run opens a fresh, fullscreen Sway workspace. Automated orbit holds the
middle button while a persistent virtual pointer supplies circular relative
motion at approximately 999 events/second. The input-only control omits the
button. The earlier timer-driven orbit was a different, lower-rate workload.

The following runs had no CPU profiler, Tracy, protocol logging or compilation
active. Each measurement covers 12 seconds after warmup. Percentages use
`100 * (process user seconds + system seconds) / elapsed seconds`, so 100%
means one CPU core. They exclude Blender and, for nested Weld, the parent Sway
process. Individual short runs establish the large effect; these are not
statistical confidence intervals.

| Test | CPU | Artifact directory under `target/validation/` |
| --- | ---: | --- |
| Weld dev, normal orbit | 60.4% | `orbit-_a5ga2rv` |
| Weld dev, orbit, validation disabled | 55.1% | `orbit-y_0kioyh` |
| Weld dev, orbit, validation disabled, Blender forced to vsync | 14.3% | `orbit-qt61zm7f` |
| Direct Sway, normal orbit | 3.0% | `orbit-03t6yfqu` |
| Weld dev, pointer motion only, validation disabled | 4.3% | `orbit-y43zeaya` |

Earlier perf-sampled runs measured 62.0%, 47.8% and 3.0% for the first,
second and fourth cases (`orbit-h3kmfx2r`, `orbit-u9566enm`, `orbit-3mlguako`).
Validation adds cost, but disabling it does not explain or eliminate the gap.
The normal run's log contains Khronos VUID messages, confirming the layer was
active; "default" alone would not establish that.
Those samples attributed 25.85% of default-run userspace samples to the
validation library. This is a sample share, not 25.85 process CPU points.

Blender's viewport is slightly smaller inside Weld (1815x1178 versus
1837x1238); this is not a pixel-identical renderer benchmark. In the unprofiled
orbit runs Blender drew roughly 180 times/second inside Weld, roughly
460 times/second under Sway, and 60 times/second with forced vsync. Sway
therefore remains cheaper even while receiving more client redraws. CPU
frequency samples showed active cores reaching approximately 4.8-4.9 GHz in
earlier captures; the old 2 GHz clamp is not required to reproduce the issue.
Weld here is the ordinary development build, and nested presentation adds an
extra compositor boundary. These numbers describe the reported development
workload, not an optimized standalone Weld-versus-Sway parity benchmark.

## Evidence: buffer starvation and Wayland roundtrip storm

Separate four-second protocol-count probes used 250 Hz pointer replay:

| Case | Root surface commits | `wl_display.sync` requests | Root frame requests |
| --- | ---: | ---: | ---: |
| Weld, normal Blender | 692 | 13,958 | 0 |
| Direct Sway | 752 | 752 | 0 |
| Weld, Blender forced to vsync | 240 | 0 | 240 |

Artifacts: `orbit-pvtnhpsx`, `orbit-lv7_j871`, `orbit-yvkm2m7e` respectively.
Counts are between the capture markers. Logging affects throughput, so these
rates must not be equated to the unlogged 1000 Hz runs above. The client reused
its existing DMA-BUF pool; repeated buffer allocation was not the activity
behind the storm. No SHM copy path was involved.

The installed Mesa version's source explains the repeated sync requests:
[`wait_for_free_buffer` in platform_wayland.c](https://gitlab.freedesktop.org/mesa/mesa/-/blob/mesa-26.2.3/src/egl/drivers/dri2/platform_wayland.c)
loops over its locked color buffers. When none is free, it performs
`wl_display_roundtrip_queue` and retries. The source explicitly describes
roundtrips as a way to force servers to flush pending releases. This was
verified against the Nix source for Mesa 26.2.3:
`/nix/store/6pnvm1jkh0a144pmcsbkykq5crhn695a-source`, lines 1260-1295 of that file.

Weld's [`DmabufManager::stage_inner`](../../crates/weld-core/src/dmabuf/manager.rs)
moves a replaced `StagedImage` into `superseded`, retaining its client lease
until `prepare_render` runs at the next composition. Keeping the import/source
identity alive protects queued ECS snapshots, but also retaining the lease
withholds a never-sampled buffer from the client's reusable pool. A small pool
can therefore become locked while waiting for the next presentation tick.

The loopback relay's commit cache replaces the previous use on the next
commit; it does not explain the continued retention of these superseded
uses. Render-image snapshots carry image/import identities, not the lease.
The manager is a concrete unnecessary lease holder on this path. Fable's
independent ownership audit reached the same conclusion.

An optimized Tracy comparison, with Tracy internal sampling disabled, shows
the amplification at the host boundary:

| Whole capture | Normal Blender | Blender forced to vsync |
| --- | ---: | ---: |
| Outer event-loop dispatches | 457,163 | 19,917 |
| Main-world advances | 752 | 765 |
| Compositions | 750 | 763 |
| Input-ingress batches | 9,967 | 10,362 |

Artifacts: `orbit-3n5mprt2` and `orbit-i200jd1y`. Counts include startup/tail,
not only the timed 12-second active region. Composition counts remain similar
while loop iterations differ by about 23x. Bevy and the tiler are not running
a complete composition for every input or roundtrip. Input-ingress work was
about 41-44 ms across each complete trace, while many tiny per-loop operations
accumulated across hundreds of thousands of iterations.

Together, the protocol counts, Mesa source, lease audit and pacing control
identify a concrete feedback loop. The benefit of changing lease retention
itself still requires an implementation and matched A/B test; forced client
vsync is a diagnostic control, not the proposed general compositor fix.

## Secondary costs and follow-up

- `NativeRuntime::serve` calls `take_keyboard_settings` every outer iteration.
  That clones and value-compares the published settings even when unchanged.
  `memcmp` accounted for 7.61% of validation-off dev userspace samples; an
  optimized frame-pointer sample's caller resolves to `native.rs:191`, this
  settings publication call. Disassembly of `take_keyboard_settings` in the
  captured perf executable confirms an unconditional full-length `bcmp` for
  equal-length present keymaps (call at `0x370f44f`), without an Arc pointer
  equality shortcut. Prefer change/revision-based publication after
  actual configuration changes. This cost is magnified by the roundtrip storm.
- Composition still has real fixed cost. The paced control uses about 14% CPU
  in dev, above direct Sway's 3% unpaced baseline. Render extraction, Bevy
  scheduling, external-image barriers and nested presentation remain future
  profiling targets. The current samples do not justify attributing all of
  that remainder to one system.
- The first fix to test is separating superseded import-identity retention
  from never-acquired client leases, including layer removal. Acquired,
  displayed and retiring buffers must retain their existing GPU-completion
  lifetime. Add regression coverage for queued snapshots, supersession,
  layer removal and multiple uses before repeating the orbit captures.
- Validate both paced and unpaced clients after that change. A compositor must
  tolerate applications committing faster than the display; enforcing vsync
  on Blender alone would conceal the general lifecycle problem.

## Measurement pitfalls

Tracy's default background sampling/symbolization materially perturbed this
machine: an idle capture consumed about one core and retained gigabytes of
symbol data. That capture (`orbit-5a0mz0i6`) and the failed initial profiler
run (`orbit-rv1ps2f_`) are excluded from performance comparisons. The orbit
runner disables Tracy sampling, sample retirement/cache/branch sampling,
context-switch and vsync capture; external perf owns CPU sampling.

Calloop wait spans include sleep. GPU/present spans can include waits. Neither
is equivalent to CPU time. The optimized traces also include instrumentation
cost, so the headline CPU table uses ordinary dev binaries without profilers.
Blender's fixture motion counter may be bypassed by its active orbit modal
handler; it is not a reliable lost-input counter. Draw and protocol counts
confirmed continuous orbit rendering.

## Implementation follow-up

The DMA-BUF manager now drops never-acquired leases on supersession and layer
removal, while retaining a deduplicated set of import IDs until queued
snapshots have been consumed. `known_sources` owns the imported texture during
that interval. Displayed, retiring and pending-GPU lifetimes are unchanged.

`KeyboardSettingsReader` now lives in `AppShell`, outside the ECS world. It
uses Bevy's `SystemState<Res<KeyboardSettings>>` change detection through
shared World access, avoiding clones and keymap comparisons on unchanged
polls. Value equality still suppresses identical replacements after a change.
An initial implementation stored the reader in a resource and used
`resource_scope` to access it. That was replaced before landing: in Bevy 0.19,
resource scope removes and reinserts a resource entity on every poll.

The subsequent DRM retest exposed missed repaints while typing in Blender and
Chrome, while foot continued to update. The physical presenter could invoke
Bevy rendering before queued surface ingress reached the main world, causing
a fresh staged DMA-BUF to be discarded against stale scene references. DRM
now defers presentation and its zero-time output wakeup until pending policy
can run first. Vblank retirement opens that opportunity immediately, while a
current scene still permits cursor-only presentation between application ticks.
The user confirmed that typing repaints correctly again after restarting with
the fix. The tests cover deferred ingress, clean cursor presentation and early
vblank readiness; physical validation was the user's DRM retest.

The following repeats use the same unprofiled 12-second, 1000 Hz held-middle
orbit. No client-vsync override or input-rate limit was added:

| Test | Before | Final implementation | Final artifact directory |
| --- | ---: | ---: | --- |
| Normal dev validation | 60.4% | 29.6% | `orbit-8w26i625` |
| Validation disabled | 55.1% | 17.1% | `orbit-kvt3w297` |

The lease-only control measured 16.3% with validation disabled
(`orbit-7999_y7m`). The intermediate resource-scope reader measured 32.5%
with validation and 21.4% without (`orbit-gzhha82p`, `orbit-o4ylql_q`). These
are single-run measurements with normal clock/workload variation; they do not
establish a precise isolated speedup for the reader change.

A four-second, 250 Hz protocol probe after the lease fix recorded 644 root
commits and 647 sync requests, with zero frame requests (`orbit-z0u9q8po`).
That is approximately one roundtrip per commit, versus 13,958 for 692 commits
before. Blender remained unpaced by frame callbacks. The original roundtrip
storm is removed without requiring client cooperation with vsync.

Validation:

- Core unit suite: 132 passed, one explicitly ignored native-socket fixture.
- Input binding integration suite: five passed, including multiple edits
  between application updates and identical-value replacement suppression.
- Distribution library suite: 23 passed, including initial configuration
  availability before the first native publication.
- Ordinary build, formatting and strict all-target Clippy for `weldwm`,
  `weld-core` and `weld-input` passed. Core checks use `test-support`.
- Fable reviewed the final lifecycle, reader and DRM scheduling implementation.
- The existing `shell_main`/`shell_render` benchmarks' missing `weld-float`
  dependency was verified against the checkpoint and restored as dev-only.
  No dependency versions changed.

The normal-validation capture still reports the same
`VUID-vkAcquireNextImageKHR-fence-10066` seen in the baseline. Forced test
teardown can still produce Blender's EGL/epoxy assertion after CAPTURE END,
also present in baseline logs; core dumps remain disabled. Neither is claimed
fixed by this slice. Remaining composition/validation overhead and a dedicated
GPU-backed snapshot-retention regression test remain follow-ups.
