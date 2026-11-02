# Compositor CPU investigation — 2026-11-02

## Live session evidence

The running release executable matches `target/release/weldwm` byte-for-byte
(SHA-256 `310a8b87da8c9c6924f6b0f968d8b770bd187a81f759935ed39322c7f7c17386`).
Initial process accounting measured 20.73% of one core, with 20.33 percentage
points on the main thread. The other running DRM session measured 16.47%.
The active session uses a 2240×1400 panel at scale 1.25, with Vulkan validation
disabled and `RUST_LOG=info`.

A 30-second attachment at 499 userspace CPU-clock samples/second collected
2,358 samples with zero lost samples. During an interior 15-second window,
process accounting measured 16.07% user CPU and 3.60% system CPU. The release
binary does not unwind reliably enough for inclusive call-tree attribution;
the instruction-pointer/leaf samples remain usable. Among those samples:

- 7.38% land directly in hashing `AssetId<SurfaceUiMaterial>`.
- 2.63% land directly in the surface binding cache's pruning iterator.
- 4.33% land in ECS table change-tick access, and 2.08% directly in Weld's
  retained-UI component-change detector.
- ECS, wgpu validation/encoding, Vulkan driver calls, allocation, and native
  host servicing contribute additional work. These categories overlap with
  higher-level callers and must not be added to an inclusive profile.

These are shares of sampled userspace CPU, not percentages of a whole core.
The live capture and flat symbol report are under
`target/validation/cpu-2026-11-02/` (`live.data`, `live-flat.csv`,
`live-leaves.txt`, `live-cpu.json`).

The user supplied a second live capture, saved as `user-live.data`. Filtering
to Weld's PID excludes a child application started during that recording. It
also has zero lost samples, with a larger copying component (9.53% in
`memmove`), 2.63% directly in surface-material ID hashing, and 2.90% in ECS
change-tick access. This reinforces that workload changes alter the mix; copying
samples alone do not establish which buffer or upload path performed the copy.

`perf record` needs `--user-callchains` in addition to `-e cpu-clock:u` on this
kernel with `perf_event_paranoid=2`. A 64-page ring works without changing
system settings. The initial 256-page request exceeded the locked-memory limit.

## Obsolete SHM binding generations

Each SHM upload advances `SurfaceBufferAsset::generation`, while retaining the
same image asset ID. `SurfaceBindingCacheKey` correctly includes that generation
because the GPU image can be replaced behind the asset ID. Its pruning predicate
previously checked only material parameters and membership in the live image-ID
set. Every previous generation therefore remained cached until a parameter,
image-ID, or material lifetime change happened to remove it. Each composition
scanned and hashed those obsolete entries again, even with unchanged content.

The existing GPU replacement regression was extended to require exactly one
binding after a pixel replacement. It failed before the fix with two bindings.
The fix publishes current image generations and prunes obsolete generations.
Imported DMA-BUF image identities retain generation zero so live buffer-pool
rotations can continue reusing their bindings. Repeated SHM replacement, pixels,
parameter-buffer reuse, hide/show, and teardown are checked together.

`shell_render` now has a bounded cache-aging mode:

```sh
cargo bench -p weldwm --profile dev --bench shell_render --no-run
WELD_RENDER_BENCH_CACHE_AGING=1 target/debug/deps/shell_render-HASH
```

It uploads small SHM images, then measures 1,000 unchanged render/completion
iterations at each history checkpoint, with no main-world updates in the timed
section. CPU time sums process user and system ticks; wall time also includes
GPU waits. This is an unpaced offscreen fixture, not a desktop utilization test.

The initial before/after/after/before sequence, without overlapping compilation,
measured the following after 4,096 uploads:

| Binary | Process CPU µs / unchanged frame | Wall µs / unchanged frame |
| --- | ---: | ---: |
| Before, first | 490 | 1,619 |
| After, first | 320 | 1,198 |
| After, second | 370 | 1,310 |
| Before, second | 660 | 1,617 |

Fresh-cache CPU varied substantially (240–690 µs/frame); the deterministic
cache bound and the repeated aged comparison are stronger evidence than a
single timing. Those initial binaries used a 900×600 initial image before
switching to 64×64 updates; the benchmark now also starts at 64×64 to keep
geometry constant across checkpoints. The 4,096-upload comparisons above all
used identical 64×64 content. Logs and saved binaries are in the same evidence
directory (`aging-before*`, `aging-after*`).

## Input and presentation controls

Production does not currently install Leafwing's `InputManagerPlugin`; its
remaining code references are an ordering declaration and tests. Native input
already forwards raw events independently of refresh-paced Bevy batches.

The renderer-free development-profile benchmark measured roughly 0.1 µs per
input batch and 49.4 µs per full shell update with three windows and 16 motion
events. It omits the actual renderer plugin assembly and physical presentation,
so it establishes only that raw projection and that policy fixture are small.

Two production retained-path offscreen runs with three SSD DMA-BUF clients at
120 Hz each and a 1080p60 output measured 12.70% and 13.50% process CPU. Both
performed zero main-world updates during measurement, maintained 60
compositions/second and about 360 commits/second, and passed pixel-equivalence
and output-target-rotation checks. These development-profile results cannot be
subtracted directly from the live release total. Evidence:
`target/validation/paced-render-c9bdmeha`.

## Hidden client activity

The active session contains a remotely controlled Artboard window with no
desktop output and `visible=false`. A bounded running/paused/resumed experiment
on that client measured Weld at 25.79%, 13.85%, and 25.41% CPU respectively.
Userspace instructions over the approximately ten-second windows were
3.552 billion, 2.087 billion, and 3.569 billion, with 100% counter coverage.
The client was restored in a `finally` block, with an independent resume watchdog.

The profiling build was compiling during this experiment, so CPU frequency and
wall-time comparisons are not controlled. The repeatable instruction-count
change establishes substantial extra compositor work from the hidden client.
The host currently requests composition on every content event, and the
application queues content without considering its explicit local presentation
activity. This identifies a scheduling boundary to correct while preserving
structural updates, resize acknowledgements, remote consumers, and remapping.
Evidence: `hidden-client-experiment.json` and `hidden-*-counters.csv`.

A second sequence kept Artboard paused while pausing/resuming Waybar. Weld
measured 13.67%, 13.14%, and 13.75%; the panel was a comparatively small factor
in that window. Both clients were resumed automatically. This does not establish
that the residual is idle cost: other clients and the interactive terminal were
still running. Evidence: `panel-experiment.json` and `panel-*-counters.csv`.

Artboard's live process holds DRM-exported DMA-BUFs (9,830,400 bytes each).
Its hidden-content scheduling problem is distinct from the SHM generation
problem. A small SHM/cursor allocation in the same process does not classify
the main window's buffers.

### Corrected local admission

`AppShell` now uses policy's explicit `SetPresentation` activity for content
admission. Stable hidden commits replace one retained snapshot without asking
the native host to update policy or compose. Unrelated visible frames also leave
those snapshots deferred. Layout, mapping, input-region and buffer-inventory
changes, control events, and a pending resize acknowledgement enter ordinary
admission. Returning visibility rearms admission of the latest snapshot without
waiting for another client commit. Independent consumers observe commits before
this local boundary and keep their own delivery and callback cadence.

Review also identified a buffer-pool constraint: the old locally displayed
DMA-BUF must retire when the first hidden replacement is retained. The final
implementation detaches only replaced layers and requests a bounded cleanup
through the existing GPU completion path. Later hidden commits need no further
retirement work, and unchanged subsurface layers retain their content. A
regression verifies one-time retirement and preservation of unchanged layers.

The `hidden` paced-render case runs real, unthrottled DMA-BUF clients against
the production host. Its surfaces are explicitly inactive and their UI nodes
are hidden. An isolated checkout of `9d05c59a`, with the same new fixture added,
provides the before control. Thus both sides already include the SHM cache fix.
Development profile, 1080p60 virtual output, three clients at 120 Hz each,
five seconds warmup, ten seconds measurement, compilation stopped:

| Run | CPU, % of one core | Desktop compositions/s | Client commits/s | Main updates |
| --- | ---: | ---: | ---: | ---: |
| Before A | 14.595 | 59.980 | 360.082 | 0 |
| After B | 3.800 | 0 | 359.994 | 0 |
| After B | 3.600 | 0 | 359.979 | 0 |
| Before A | 11.993 | 59.865 | 359.788 | 0 |

Mean CPU falls 72.2% in this fixture while client progress is unchanged. The
remaining cost services incoming protocol/adapter work; it is not Bevy layout
or local rendering. These percentages are not a prediction for the live
desktop's mix of clients, scaling, input and DRM presentation. The initial
fixture printed a meaningless per-composition CPU value when no compositions
occurred; that field now prints `n/a` and the CPU totals above are unaffected.

Evidence directories, in order: `paced-render-dczvmerm`,
`paced-render-07m3bx4j`, `paced-render-kntzjytf`, `paced-render-m7zqjg4e`.
Commands and summaries are retained as `hidden-abba-*.log`; saved executables
are `hidden-before` and `hidden-after` under the main evidence directory.

An empty nested instance of the original release binary, using the launch-free
fixture configuration, measured 0.467% CPU over fifteen seconds after warmup.
That rules out a universal 20% empty-scene floor in that backend. It is not an
empty physical-DRM measurement. Evidence: `nested-idle.json` and its log.

## Optimized pipeline isolation

The final matrix uses the optimized `perf` profile, the same RADV device,
2240×1400 at scale 1, three 120 Hz SSD clients, and a 60 Hz output. Each case has
five seconds warmup and eight seconds measurement. The second pass reverses the
case order. All compilation finished before measurement. Hardware counters
cover an interior approximately six-second window, with 100% coverage; process
CPU covers the full measurement window and includes kernel time. Producers
are excluded from both.

| Control | CPU %, first / second | Userspace million instructions/s, first / second |
| --- | ---: | ---: |
| Direct surface draw and native buffer lifecycle | 6.125 / 6.125 | 34.02 / 34.58 |
| Also renderer-equipped main schedule, no client entities | 9.498 / 8.875 | 46.65 / 45.73 |
| AppShell state/import, direct draw | 9.375 / 9.245 | 49.08 / 48.59 |
| Also extraction, direct draw | 11.499 / 11.998 | 54.16 / 53.82 |
| Also render preparation, camera graph gated off | 10.875 / 14.248 | 66.83 / 66.16 |
| Production retained Master, no input | 12.367 / 8.875 | 70.43 / 71.41 |
| Full-update/full-preparation Master control | 14.125 / 14.124 | 111.18 / 111.84 |
| Retained Master with 1000 Hz pointer input | 15.250 / 12.625 | 120.79 / 121.10 |

Every case sustains approximately 60 compositions and 360 commits per second.
The retained no-input cases perform zero main-world updates. The 1000 Hz cases
deliver 8,000 events in each eight-second window but perform only 480 main-world
updates: application input is already refresh-paced. Continuous input restores
the full application's work at that cadence; it does not execute Bevy at mouse
polling frequency. Leafwing's manager plugin is not installed in this assembly.

Instruction rates repeat much more closely than CPU times. The controls show
separate costs in raw protocol/buffer servicing and direct drawing, main-world
framework scheduling, extraction, and render preparation. Master's full
preparation adds further work that retained content avoids. Bevy is rendering
client textures and chrome, maintaining render assets/views/bindings and
submitting GPU commands; it is not used solely for layout. These diagnostic
draw streams and assemblies differ, so their deltas are not an additive
per-function decomposition of the live desktop.

`matrix-summary.json` records counts, rates and each evidence directory;
`matrix-*.log` retains the launch summaries. An optimized stack
investigation was also attempted (`retained.data`). The installed perf/libdw
unwinder reports `address range overlaps an existing module`, including with
full dependency symbols. Its incomplete call trees are not used for inclusive
CPU attribution. The live instruction-pointer samples and causal workload
controls above do not depend on successful stack unwinding.

## Conclusions and scope

The sustained desktop load combines real client-ingress/rendering work with
two demonstrated avoidable costs: hidden consumers requesting local composition,
and obsolete SHM bindings increasing later-frame work. Both are corrected.
Replacing input libraries or adding device-rate sleeps is not supported by
these measurements. Raw client input remains immediate and application input
remains paced. Explicitly inactive workspace windows, controlled offscreen
windows and fullscreen-occluded windows use the same admission rule; independent
remote consumers retain their cadence.

The fixes were measured in real-Wayland offscreen fixtures and GPU integration
tests. The user's running DRM session was profiled and temporarily isolated,
but was not replaced or restarted. A production CPU figure after restarting
that session must be measured separately; the benchmark savings are not a
claim that the already-running binary has changed.

## Validation

The cache fix passes all 23 shell tests, including GPU rendering, fractional
scaling, retained preparation, target rotation, clipping, pixels, resize, and
teardown. The broader dependency-lint invocation stops at
`weld-diagnostics/src/report.rs:42` (`chunks_exact_to_as_chunks`); that file is
unchanged from the baseline. Smithay also reports its existing `fetch_update`
deprecation, verified in the initial build before edits. A focused Clippy pass
additionally found the unchanged benchmark callback field's `type_complexity`;
it now has a named function-pointer alias.
The final benchmark compiles. Strict Clippy still stops on baseline code:
the existing pixel-comparison test's `chunks_exact(4)`, a nested conditional in
`src/master/mod.rs`, and three `ok().expect()` calls in remote-control tests.
The production `weld-app` library passes the dependency-free strict lint pass;
no lint suppressions were added.

Final validation passes 329 library tests across `weld-app`, `weld-core`,
`weld-window`, and `weldwm`; 51 opt-in core tests remain ignored by that command.
The desktop all-target compilation and formatting checks pass. Real Wayland
policy-only, stalled-presenter, presenter-reclaim, and nested capture checks
also pass (`native-regressions.log`).

The final buffer-retirement protocol trace is
`target/validation/paced-render-sg_t8ag3/hidden/run.log`: the first displayed
buffer is released during warmup, then three allocation IDs continue rotating
and receiving releases throughout measurement. The client produces 481 DMA-BUF
commits over four seconds with zero desktop compositions or main updates.
This trace is a lifecycle check, not a timing comparison; Wayland debug logging
was enabled. The unchanged-layer/one-time-retirement regression also passes.
The final untraced three-client smoke run, including that retirement correction,
measures 4.375% CPU, 359.978 commits/second, zero main updates and zero desktop
compositions (`paced-render-h1gfvhhl`, `hidden-final.log`).

## Visible video and keyboard-focus follow-up

After login, `/proc/2593748/exe` matches the installed `cb0a13ec` binary
(SHA-256 `ce05ad4f79b75248b558d91fd69c64615acdd30244539cbde7275e6640e51edb`).
Both `weld-session.target` and `graphical-session.target` are active. A new
ten-second process sample measures 12.80% CPU, including 12.30% on the main
thread. The following artifacts are under
`target/validation/visible-cpu-2026-11-02/`.

The user kept a playing browser video and an htop terminal visible for a
30-second measurement. Weld consumes 9.16% user CPU and 2.42% kernel CPU,
11.59% combined. The 999 Hz userspace capture contains 2,817 samples with
zero losses. Its exact library-level sample shares are 81.90% in the statically
linked executable, 8.98% in RADV, and 8.31% in libc. The executable includes
Weld, Bevy, wgpu and Smithay; its share is not all application code. Flat
symbols include ECS scheduling/component lookup, retained-scene checking,
layout, native protocol servicing, wgpu command processing, copying and
allocation. No individual leaf dominates. The kernel share remains unattributed
by this userspace capture. Incomplete DWARF stacks are still unsuitable for
inclusive caller attribution. See `video-cpu.json`, `video-libraries.csv`,
`video-symbol-counts.csv` and the start/end window inventories.

### Core placement changes CPU time substantially

This Ryzen AI 9 365 exposes logical CPUs 0–7 with a maximum frequency near
5.09 GHz and CPUs 8–19 near 3.32 GHz. Both groups use the `powersave` governor;
maximum frequency is not a measurement of actual instantaneous frequency.
An ABBA comparison pins only an isolated benchmark's main thread, after its
producer processes have started. The live compositor's affinity and system
power policy are unchanged. Each case uses the optimized current baseline,
three real DMA-BUF producers at 60 Hz, 2240×1400 output at 60 Hz, four seconds
warmup and ten seconds measurement. All compilation finishes first.

| Main thread | Process CPU, % of one core | Userspace million instructions/s |
| --- | ---: | ---: |
| CPU 4, A | 6.00 | 60.71 |
| CPU 8, B | 10.10 | 60.94 |
| CPU 8, B | 9.70 | 60.46 |
| CPU 4, A | 5.60 | 61.85 |

All four runs sustain 59.90 compositions/s and about 180 client commits/s.
Nearly equal instruction counts with a roughly 70% CPU-time difference show
that utilization changes need not mean more compositor work. This experiment
combines core choice with the machine's ordinary frequency/cache/scheduling
behavior; it does not isolate maximum clock rate as the sole cause.

Direct-draw controls, which retain native protocol/import/buffer lifecycle but
omit the production Bevy scene, use 3.00% CPU / 25.20 million instructions/s
on CPU 4 and 3.90% / 24.66 million on CPU 8. The production retained scene is
therefore materially more expensive even without frequent policy updates.
These differing render assemblies are causal controls, not an additive
per-function decomposition. The normal path still executes Bevy's render
lifecycle and redraws the output; partial Bevy scene damage and primary client
direct scanout remain unsupported, as recorded in `docs/drm-presentation.md`.
Native client acquisition/retirement and output acquisition/release also have
their own GPU submission boundaries around Bevy rendering.

### Keyboard focus adds work as well as exposing placement variation

The user reproduced keyboard focus changes at roughly 2–3 Hz between the
browser and htop. A 25-second recording sees per-second CPU reach 16–20%.
The median interval is 15.82% CPU and 104.59 million userspace instructions.
A subsequent 15-second observation, with the browser focused throughout,
has medians of 12.85% and 90.47 million instructions. There are transient
instruction spikes in both windows; this live comparison is not a constant
video-content microbenchmark. The recorded last-running CPU frequently moves
between the two groups. These once-per-second CPU IDs are endpoint observations,
not full residency traces. User-only migration/context-switch counters returned
zeros and are not evidence of no migrations.

The isolated fixed-core controls distinguish the additional work:

- One keyboard focus change/s on CPU 4 uses about 62–63 million instructions/s
  and 6.1–6.2% CPU before the color change below, versus roughly 61 million
  and 5.6–6.0% without focus changes.
- Three changes/s on CPU 8 use 65.28/66.00 million instructions/s and
  10.79/9.70% CPU in the two baseline runs.
- One tab switch/s, which also changes visibility/layout, uses 80.48 million
  instructions/s and 7.40% CPU on CPU 4. Tabs exercise related visibility work;
  this is not a direct workspace-switch measurement.

Both real work and placement variation contribute. Replacing Leafwing or
limiting mouse input would not address this keyboard reproduction.

### Border-only preparation

A diagnostic focus trace identifies two changed `BorderColor` components,
followed by full UI preparation and a further preparation frame even when
there are no new scene, image or pipeline changes. Border colors are extracted
directly into the current frame's vertices, so this extra asset-settling pass
is unnecessary. Border-only invalidation now prepares once. Asset changes,
structural changes, other component changes, pending pipelines and image
registration changes retain their previous settlement behavior.

The GPU regression fails before the change on the unwanted second preparation
and passes afterward, checking immediate new pixels, retained replay and
equality with full preparation. A focused policy regression checks concurrent
asset changes and border removal. The application/distribution library suites
pass 97 + 41 tests before adding that extra policy test; the final focused run
passes both new regressions. Formatting and diff checks pass. Strict Clippy
passes the changed application library but stops at the unchanged nested `if`
in `src/master/mod.rs:398`, also present in `cb0a13ec`.

The change removes proven redundant work, but no meaningful aggregate CPU
improvement is established: at 3 Hz the after runs measure 66.27/64.93 million
instructions/s and 9.80/10.20% CPU, overlapping the baseline. At 1 Hz the
after runs also overlap normal variation. Diagnostic runs include unrelated
image asset/registration changes that still require preparation; not every
observed preparation belongs to a focus change. This small correction is not
a solution to the entire steady video cost. It is committed source work and
has not replaced the running executable.

The first exploratory `steady.log`, `focus.log`, `tabbed.log` and
`focus-trace.log` used an older saved profiling executable. They are excluded
from the current-build comparisons above. `baseline-bench` was rebuilt from
`cb0a13ec`; `after-bench` includes the border-only correction. The `core*`,
`focus-before/after*`, `focus3-*`, `tabs-*` and `direct-*` logs record the
current-build controls and their artifact directories. No timing comparison
runs concurrently with compilation or diagnostic tracing.
