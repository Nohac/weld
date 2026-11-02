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
