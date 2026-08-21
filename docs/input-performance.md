# Input performance

**Status: Allocation and atomic cursor-plane passes implemented; live DRM validation pending.** This note
separates device-paced input delivery from refresh-paced application work and
physical presentation. It records the current implementation and measurements;
it is not a claim that the remaining costs are acceptable.

## Required behavior

Raw input remains ordered and is forwarded to the focused Wayland client at
device pace. The application receives the same input at the output update
cadence so picking, shell interaction, Leafwing state, and cursor policy do not
run at a 1,000 Hz device rate. Refresh pacing may coalesce application-facing
pointer motion, but must not delay or discard client-directed motion, button,
axis, gesture, or keyboard transitions.

The host loop is single threaded. A lock-free queue does not improve that
ownership model. A fixed-capacity ring buffer is also the wrong general input
primitive because overflow cannot safely discard a key or button transition.
Use retained queue storage for lossless discrete events and replace only
consecutive application-facing pointer-motion samples when their ordering is
not separated by a discrete event.

## Measured behavior

Controlled Tracy scenarios used an optimized but instrumented release build.
The instrumentation materially perturbs a high-frequency compositor path, so
zone counts and relative comparisons are evidence while recorded CPU
percentages are not production measurements.

| Workload | Converted motion events | Application advances | Physical compositions |
| --- | ---: | ---: | ---: |
| One output, rapid touchpad | about 135/s | about 55.5/s | about 54.1/s |
| Two outputs, slow external mouse | about 225/s | about 50.0/s | about 40.1/s |
| Two outputs, rapid external mouse | about 925/s | about 52.2/s | about 46.2/s |

The rapid external-mouse trace contained about 744 libinput wakes per second,
with roughly 1.24 converted events per wake. Conversion and immediate host
delivery scaled with that rate but did not explain the lost physical frames.
The two-output trace instead showed one application advance producing two
output passes, completion waits, and KMS submissions, while busy batch targets
prevented every advance from becoming a physical composition.

Those Tracy captures predate the active hardware-cursor path: continuous motion
then dirtied composition, with the dirty bit coalescing requests to the output
cadence. An idle pointer still produces only startup compositions and then lets
the host loop sleep. Weld does not render one scene per raw device event.

A later uninstrumented release run with the DRM runtime counters observed a
two-output rapid external-mouse interval at about 983 raw events, 980
input-bearing dispatches, 1,042 host-loop iterations, exactly 60 application
updates, 0.2 compositions, and 984 successful legacy hardware-cursor moves per second.
The hardware cursor remained active with no fallback, while the post-ECS cursor
synchronization pass still ran once per host-loop iteration. This is one TTY
observation of rates, not CPU attribution. The conditional cursor-sync slice
targets that redundant second evaluation only. The subsequent atomic cursor
slice replaces those per-event ioctls with one Smithay-owned KMS transaction in
flight per output. Cursor updates overwrite desired state while that transaction
is pending, and the newest state is merged into a queued primary frame or sent
as a cursor-only atomic commit.

The initial atomic policy favors pointer latency: when no completed primary
frame is queued, a cursor-only commit may start while wgpu is still rendering a
leased primary target. A primary frame that completes immediately afterward can
therefore wait up to one refresh interval for that cursor commit to retire.
Measure composition-to-scanout latency alongside cursor commit rates. If this
cost is visible, defer behind genuinely active rendering with a bounded
one-refresh escape rather than allowing an abandoned lease to freeze cursor
submission.

## Current allocation audit

The raw pointer payload contains scalar data. Libinput conversion returns two
fixed optional slots rather than allocating a collection. Input `VecDeque`s and
Bevy message vectors retain capacity after growth, and cloning a pointer event
does not clone heap-owned data. Keyboard logical-key payloads can own data, but
they are not part of the high-rate pointer path.

The first allocation pass removed the source-audited hot-loop churn. Physical
output IDs are cached until connector or mode state changes. Presenter
readiness is rebuilt into retained scratch storage immediately before each use,
and acquired frames, composition requests, and completed output frames reuse
loop-owned buffers. Composition duplicate validation uses the normally tiny
borrowed output slice without allocating a temporary set.

The application-facing input path uses a growable `VecDeque` with capacity for
an ordinary 64-event burst. Consecutive absolute pointer motions replace the
queue tail, while buttons, axes, gestures, keys, focus changes, and pointer
leave events remain lossless ordering barriers. The projection schedule borrows
each event while producing Bevy messages, then moves the same event into the
retained routing queue instead of cloning it. Immediate Smithay forwarding is
unchanged and still sees every unconsumed raw event.

This is source-audited churn, not an allocator profile. Measure an
uninstrumented release with a sampling or allocation profiler before assigning
it a CPU percentage.

## Optimization order

1. Done: cache physical output IDs and reuse caller-owned readiness storage,
   while evaluating session and presenter lifecycle state at each use.
2. Done: retain scratch storage for composition requests, acquired frames, and
   completed output frames; validate the borrowed output slice without a set.
3. Done: reserve ordinary input bursts and coalesce only consecutive
   Bevy-facing pointer motions. Preserve every discrete transition and all
   immediate client delivery.
4. Done: borrow an event while projecting it and then move it into retained
   state, avoiding clones whose payload may own data.
   The next measured slice also makes the post-ECS cursor synchronization pass
   demand-driven instead of running on every input-driven host iteration.
5. Done: put the compositor-normalized cursor image on a DRM cursor plane when the
   hardware accepts it. Position-only changes then update plane state without
   leasing or repainting the primary scene. Keep GPU cursor composition as a
   capability fallback.
6. Give each output independent dirty, readiness, and cadence state so cursor
   motion on one output cannot wait for or recompose another output.

The existing headless application and render benchmarks remain useful lower
layers. They deliberately omit calloop, GBM leasing, cursor-plane or cursor-pass
work, Vulkan ownership transfer, presenter completion, and KMS submission, so
they cannot by themselves explain end-to-end DRM utilization.
