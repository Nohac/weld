# Receiver decoder pooling

Status: implemented with GPU-free validation and a successful user WAN stress
run. The measurements do not establish the hardware decoder's capacity or a
controlled before/after speedup.

## Ownership and scheduling

`weld-hoist-encoded` owns ordered per-surface commits, pending media, decode job
tokens, cancellation and atomic application. Its backend interface returns Busy
with the original owned request. It does not expose worker counts or force the
port to guess whether a backend has room. Each pass offers at most one job per
ready surface, then rotates after the last successful submission. Passes repeat
while submission or application makes progress. The front commit and at most
one compatible successor are eligible; only the front is applied. Lookahead
requires all earlier replacements to be submitted or decoded, mapped commits
in the same session, unchanged presentation topology/metadata and successor
replacement stream/generation keys present in the front replacements. Missing
media, metadata-only commits, unmap, new layers and generation changes stop it.

`weld-media-vaapi::VaapiDecodeWorker` is the nonblocking facade over a lazy pool.
Default limits are four workers, eight outstanding jobs and sixteen owned stream
generations shared across that connection. `DecodePoolLimits` can lower those
limits; they are not discovered hardware capabilities or a physical-device-wide
allocator. Workers grow only when real work arrives. A stream stays assigned
while any of its generations is owned, including retiring generations. Native
FFmpeg devices, decoder contexts and VPP converters are created and used on
their worker thread; no unsafe thread-transfer assertion is added.

Each worker accepts up to two outstanding jobs, including completed but undrained
results. Its explicit depth (one or two) also reserves that many extra caller-held
hardware frames in FFmpeg before the decoder opens. The worker submits available
packets before finishing the oldest hardware frame, then refills its pipeline.
It never waits to fill a batch: a single frame progresses immediately. This
overlaps already-available work, including consecutive frames of one stream,
without requiring a host drain between those two jobs. Submission and completion
remain FIFO per worker; this is not a multi-picture GPU submission API.
Unassigned parked
workers are reused before spawning. At the cap, new streams use an available
worker with the fewest owned generations. Idle threads remain until connection
drop, but obsolete codec contexts retire promptly. There is no thread migration
or early thread shrinking.

## Retirement, cancellation and progress

Current inventory, queued undecoded references, pending media and active jobs
protect a generation. A completed result owns freshly converted, synchronized
XRGB storage independent of the decoder's reference-picture pool. Thus an
obsolete context can retire even while its XRGB output waits for other layers
of an atomic commit. This distinction allows a sixteen-layer resize to progress
without simultaneously requiring thirty-two decoder contexts. Renderer-owned
leases still preserve their independent allocations until normal release.

The pool owns generation reservations; native code never implicitly evicts an
older generation. Retirement is keyed and idempotent. Requests for unowned keys
are immediate no-ops. Owned reservations remain charged until an explicit
worker retirement acknowledgement, even if native initialization failed before
creating the context. A matching active job delays retirement publication.
Cancelling a surface marks its jobs obsolete but retains them until completion;
late results cannot resurrect the surface.

A coalescing retirement set is published before a bounded wake command. A
SeqCst flag permits only one queued Wake, and the channel reserves depth plus
one slots so wakes cannot consume the decode budget. The worker clears that
flag before locking the set, checks retirements before blocking and after each
received command. Receiving even a stale
wake signals host capacity recovery directly. Completion, retirement and worker
failure also wake the host; a static last pending frame needs no new input or
client commit to retry. Busy capacity is not inferred to be fatal from a local
snapshot: retirement acknowledgements or media/control can still arrive.

The result channel uses an unbounded channel type but cannot accumulate arbitrary
events: eight outstanding jobs, sixteen owned retirement keys and four one-shot
worker failures bound it to twenty-eight events at the default limits. Wake
receipt does not enqueue a result event. Initialization errors and panics become
sticky terminal failures, not per-frame worker respawns. Draining returns all
available completions alongside an optional failure so another worker's error
does not hide completed jobs or break cancellation accounting. Per-job submission
errors occupy their FIFO slot rather than overtaking earlier results. A poisoned
native generation rejects further submissions into ordered error slots; its
context is reset only when its pending native FIFO entries have drained. Pending
AVFrames own their references independently and drop before codec/device fields.

## Limits and measurements

The existing 128-reference/output bound and compressed-byte admission remain.
These are not a GPU byte-residency budget: codec DPBs, padded frames, VPP output
and renderer-held leases have additional costs. Lookahead can approximately
double decoded-image residency per surface; extra hardware-frame slots also
increase decoder memory requirements. Multiple connections each have
their own pool. A topology that cannot fit its required generations can remain
Busy; a future admission controller must reject or adapt it with actual budget
knowledge. Larger queues and speculative fatal-capacity checks are not used.
Drop closes command queues and joins workers. Once closure is observed, remaining
native frames are released without XRGB conversion. A full pipeline can finish
its front job before observing closure; an already-running native GPU call that
never returns still cannot be recovered by this pool.

Receiver summaries report worker queue wait, residence, completion-to-host-drain,
submission wall, pending-before-finish, finish wall and outstanding job count.
`worker_residence` replaces the former `worker_execution` name because it now
includes time overlapping other jobs. `overlapped_submissions` counts successful
jobs submitted with an earlier native job pending, not measured GPU concurrency.
These are local
`Instant` measurements, never wire timestamps. Existing `decode_wall` includes
the whole service path. None of these are GPU-only counters; overlapping job
durations must not be summed as device/CPU utilization. Debug ownership records
report worker/generation distribution on ownership changes, not every frame.

## Validation

Real worker threads with a fake processor test lazy growth, affinity, shared
limits, repeated sixteen-stream generation rotation, retirement while busy,
full wake queues, sticky initialization failure, panic/completion ordering and
shutdown without a consumer. Fake-codec port tests cover multi-layer atomicity,
cross-surface fairness, preserved Busy ownership/timestamps, cancellation of
several jobs, generation rotation with independently retained output and timing.
Deferred fake retirement acknowledgements exercise Busy-to-ready port recovery
without new packets; separate real-thread tests exercise retirement notification.
These are complementary tests, not one end-to-end host-wakeup test. Re-enqueue
after destruction and withdrawal checks the ready-queue membership invariant.
These validate Weld policy and lifecycle, not FFmpeg or Smithay implementations.

Re-run the existing AV1 local and isolated network hoist scripts with Blender
and Firefox video. Compare the same workload's pending media, media/commit age,
worker residence and completion-handoff timings against the ACK-free baseline.
Check menus, resize, reclaim and disconnect. Do not infer GPU capacity by dividing
a multi-stream peak by an unrelated whole-run decode average. No transport,
encoder pacing, protocol revision, network ACK or dynamic bitrate change is
part of this slice.

For a bounded native comparison, run `scripts/run-vaapi-roundtrip-probe`. It
pre-encodes eight identical inputs per codec, replays depths one, two, two, one
with pass labels to compare both arm orders,
then validates every output's timestamp, size, format and sampled pixels. The
last prefetched frame is explicitly finished. Each frame reports submission,
pending, decoded-surface sync, VPP setup/submission and VPP output-sync wall time.
Pixel readback and printing are outside replay timing. Frame zero is marked as
startup; this small, always-backlogged test is not a steady-state 60fps benchmark.
Depth one reserves one extra hardware frame and is therefore not byte-identical
to the previous binary. Repeat runs before interpreting small differences.

The pipeline retains both explicit GPU waits, the separate VA displays, fresh
XRGB allocations and per-frame VPP contexts. It changes when waits occur relative
to later decode submissions, not their correctness requirements. Packet wrappers
are reused, but padded compressed payload allocation/copy remains. Output/context
recycling, transfer-buffer reuse and fully asynchronous conversion are separate
work. Stage measurements will determine their priority. GPU utilization or a
speedup is not guaranteed when no next frame is available. No hardware run of
the decode-ahead change has been performed by the agent.

## September 7 WAN validation (before decode-ahead)

Run `network-hoist-tmk2tipk` used AV1 over the isolated Wi-Fi/5G setup. The user
reported much snappier Blender camera movement alongside BBB 4K60 in a Firefox
popout and htop, with occasional skips. The pool grew to four workers, and
receiver summaries sampled up to three outstanding jobs and seven streams.
This does not measure how often admission hit the worker or generation limits.

Across 110 receiver summaries from 20:51:38.825 to 20:53:28.040 UTC, 14,090 layer
decodes completed with zero codec failures. The busiest completion interval
reported 228 layer frames, not 228 fps for a single window. Average local
`decode_wall` was 7.365 ms: worker queue wait 0.013 ms, execution including VPP
5.466 ms, and completion-to-host-drain 1.885 ms. Queue wait starts at accepted
worker enqueue, not media ingress; it excludes time spent waiting for admission.
The earlier run's 7.093 ms whole-service average is not directly comparable to
the new execution-only figure, and the workloads were not controlled.

Transient latency remains substantial. At 20:52:51.983 UTC, 251 layer frames
arrived and 223 completed in the interval; 34 media frames remained queued.
The next interval received 159, completed 190 and reduced that queue to four.
Maximum media wait was 258.496 ms and commit-to-application time was 264.468 ms.
Those independent maxima are not necessarily the same frame and do not measure
actual presentation. The prior run peaked at 55 queued media frames and
834.437 ms commit age, but that is context, not a controlled speedup claim.

Source summaries counted 2,404 pre-encode coalesced commits. Those are superseded
states intentionally not encoded, not receiver frame losses. Sampled source
transport queues were empty, but samples cannot rule out brief backpressure or
buffering inside QUIC. Selected-path snapshots used direct IPv4 with sampled
RTT around 8-76 ms. Receiver admission, host handoff, source coalescing and
network bursts remain candidates for the skips; the logs do not identify each
missed presentation or justify blindly growing the pool.

The configured test duration was 120 seconds after startup. Logs ran from about
20:51:27 through 20:53:28 UTC; systemd recorded successful service deactivation
at 20:53:30, 131.429 seconds after service start including setup. This was not
a 120-second shutdown stall. No hoist transport failure or GPU reset appears
in the run logs; client connection-loss messages at teardown follow compositor
shutdown. Existing Vulkan acquisition-fence validation messages remain and are
not resolved by this slice.
