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
while submission or application makes progress. Only the front commit of each
surface is eligible; independent layers of that commit can run together.

`weld-media-vaapi::VaapiDecodeWorker` is the nonblocking facade over a lazy pool.
Default limits are four workers, four outstanding jobs and sixteen owned stream
generations shared across that connection. `DecodePoolLimits` can lower those
limits; they are not discovered hardware capabilities or a physical-device-wide
allocator. Workers grow only when real work arrives. A stream stays assigned
while any of its generations is owned, including retiring generations. Native
FFmpeg devices, decoder contexts and VPP converters are created and used on
their worker thread; no unsafe thread-transfer assertion is added.

Each worker accepts one outstanding job, including a completed but undrained
result. A lone stream still runs serially; this slice does not add same-stream
prefetch or batch several pictures into one GPU submission. Unassigned parked
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

A coalescing retirement set is published before a bounded wake command. A full
wake queue means a queued command will check the set. Receiving even a stale
wake signals host capacity recovery directly. Completion, retirement and worker
failure also wake the host; a static last pending frame needs no new input or
client commit to retry. Busy capacity is not inferred to be fatal from a local
snapshot: retirement acknowledgements or media/control can still arrive.

The result channel uses an unbounded channel type but cannot accumulate arbitrary
events: four outstanding jobs, sixteen owned retirement keys and four one-shot
worker failures bound it to twenty-four events at the default limits. Wake
receipt does not enqueue a result event. Initialization errors and panics become
sticky terminal failures, not per-frame worker respawns. Draining returns all
available completions alongside an optional failure so another worker's error
does not hide completed jobs or break cancellation accounting.

## Limits and measurements

The existing 128-reference/output bound and compressed-byte admission remain.
These are not a GPU byte-residency budget: codec DPBs, padded frames, VPP output
and renderer-held leases have additional costs. Multiple connections each have
their own pool. A topology that cannot fit its required generations can remain
Busy; a future admission controller must reject or adapt it with actual budget
knowledge. Larger queues and speculative fatal-capacity checks are not used.
Drop closes command queues and joins workers; a native GPU call that never
returns still cannot be recovered by this pool.

Receiver summaries add worker queue wait, execution (decode plus conversion),
completion-to-host-drain timing and outstanding job count. These are local
`Instant` measurements, never wire timestamps. Existing `decode_wall` includes
the whole service path. Execution is not GPU-only time. Debug ownership records
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
worker execution and completion-handoff timings against the ACK-free baseline.
Check menus, resize, reclaim and disconnect. Do not infer GPU capacity by dividing
a multi-stream peak by an unrelated whole-run decode average. No transport,
encoder pacing, protocol revision, network ACK or dynamic bitrate change is
part of this slice.

## September 7 WAN validation

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
