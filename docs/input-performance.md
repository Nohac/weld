# Input performance

**Status: Verified evidence and agreed optimization direction.** This note
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

An idle pointer produces only startup compositions and then lets the host loop
sleep. Continuous motion marks composition dirty repeatedly, but the dirty bit
coalesces those requests to the output cadence. Weld does not render one scene
per raw device event.

## Current allocation audit

The raw pointer payload contains scalar data. Libinput conversion returns two
fixed optional slots rather than allocating a collection. Input `VecDeque`s and
Bevy message vectors retain capacity after growth, and cloning a pointer event
does not clone heap-owned data. Keyboard logical-key payloads can own data, but
they are not part of the high-rate pointer path.

High-rate input nevertheless causes allocation indirectly because every host
wake reruns output selection. The DRM loop currently constructs fresh
`BTreeSet`s for physical and presentable outputs before dispatch and repeats
both collections after dispatch. That is approximately four small set
allocations per wake, or roughly 3,000 constructions per second in the measured
fast-mouse trace. Presenter handling and composition add temporary output,
request, frame, and duplicate-detection collections.

This is source-audited churn, not an allocator profile. Measure an
uninstrumented release with a sampling or allocation profiler before assigning
it a CPU percentage.

## Optimization order

1. Cache connected and presentable output lists. Update them only when udev,
   session, or presenter lifecycle state changes, and pass borrowed slices
   through the hot loop.
2. Retain scratch storage for composition requests, acquired frames, and
   validation instead of constructing collections for each frame.
3. Reserve ordinary input bursts and coalesce only consecutive Bevy-facing
   pointer motions. Preserve every discrete transition and all immediate
   client delivery.
4. Borrow an event while projecting it and then move it into retained state,
   avoiding clones whose payload may own data.
5. Put the compositor-normalized cursor image on a DRM cursor plane when the
   hardware accepts it. Position-only changes then update plane state without
   leasing or repainting the primary scene. Keep GPU cursor composition as a
   capability fallback.
6. Give each output independent dirty, readiness, and cadence state so cursor
   motion on one output cannot wait for or recompose another output.

The existing headless application and render benchmarks remain useful lower
layers. They deliberately omit calloop, GBM leasing, cursor-plane or cursor-pass
work, Vulkan ownership transfer, presenter completion, and KMS submission, so
they cannot by themselves explain end-to-end DRM utilization.
