# ACK-free encoded hoisting

Status: implemented and automatically validated; the first WAN run exposes
receiver backlog that remains a separate performance issue.

Remove application-level commit acknowledgements, not QUIC reliability, native
buffer release, or cursor acknowledgement. Per-surface stop-and-wait previously
put encode + delivery + decode + return-path time on every frame's critical
path. The September 6 stress run measured about 4.4 ms encoding, 6.5 ms decoding
and 37.6 ms commit acknowledgement turnaround in its busy segment.

## Correctness unit

1. Independently bounded control/media receive admission in Unix and Iroh.
   Pause reading when full, preserving framing and FIFO. Account referenced
   pending frames and cancelled-frame tombstones together (128 slots). Admit
   a computed control batch reserving 16 frame references per record; no deferred
   control slot is needed. Media uses independent record/byte room;
   tombstones must not suspend the media that resolves them. Keep the existing
   32 MiB access-unit validation limit and a 64 MiB queued-payload bound.
2. Two ordered source output queues. Every control record, including mapping,
   withdrawal and cursor state, uses the control queue. Busy admission returns
   the owned unsent packet to its channel's front. No blocking compositor calls.
   Encode new batches while transport media backlog is below four records and
   8 MiB, rather than waiting for an empty transport or a remote ACK. Maintain
   one active encode batch and at most one completed unsent batch; each commit
   already has a 16-replacement bound. Capacity recovery wakes the host even
   when clients are static. Coalesce superseded unencoded commits, preserving
   unobserved content and unmap/remap/structural barriers.
3. Decoder generation retirement follows references, not receipt of later
   metadata. Keep inventories current, defer retirement while a generation is
   referenced by queued control, pending media, an active decode or
   decoded-but-unconsumed output, and sweep on every poll including cancellation.
4. Delete commit ACK records, outcomes, credit gates and credit-specific
   observations; bump the exact protocol revision to 4. Update tests and current
   architecture/budget documentation together. Preserve native and cursor flows.

## Ordering and cancellation

Control admission is a prefix of published control records. Media for earlier
control records precedes media for later unadmitted ones. Both source channels
are FIFO, but either channel can progress independently of the other.

Never purge completed/published batches' media on surface cancellation. The
receiver may already hold their frame references; cancelled references become
tombstones that late reliable media resolves. An unpublished unfinished encode
batch can still be cancelled with its existing generation retirement. A peer
disconnect discards pending session work and completes existing cleanup.

## Bounds and limitations

These are memory/admission bounds, not a low-latency guarantee. The retained
32 MiB maximum access unit is a sanity limit, not an expected packet size. A
64 MiB payload queue represents roughly 67 seconds at 8 Mbps if it ever fills;
the simultaneous record bounds ordinarily constrain it much earlier. An 8 MiB
source byte threshold likewise is not an interactive latency target. Decoded
XRGB output also consumes GPU memory: one 4K frame is about 32 MiB, and 128
referenced frames across many multilayer surfaces could be several GiB. Avoid
decoding future commits ahead of each surface's front commit.

Limits apply to their named stages, not total process or GPU memory. The source
can retain one completed 16-layer batch outside transport admission (up to
512 MiB at the wire sanity limit). Iroh's raw media inbox reserves two slots
before reading bodies; Unix bounds its raw media inbox to two descriptors.
Unix kernel socket queues retain sealed-file descriptors in addition to the
userspace byte budget; that storage is kernel-queue bounded, not charged by the
userspace counter. No socket-buffer or QUIC-window tuning is included here.

Keep Iroh's existing QUIC configuration in this slice. Queue-write completion
means accepted locally, not received or presented. BDP-based send-window tuning,
age-based stream restart, adaptive bitrate and hardware budgeting remain later
work. Persistent non-coalescible structural/control overload remains a terminal
safety bound, not permission to lose releases or lifecycle transitions.

## Verification

The affected tests pass (134 with the VA-API feature enabled), as do strict
Clippy checks for the five changed crates, formatting and the debug build.

Fake-codec and real loopback/Unix binding tests must demonstrate multiple
same-surface commits with no reverse ACK; Busy-to-writable progress for static
clients; exact FIFO across retained commit and withdrawal; latest-content
coalescing; delayed multigeneration decode; independent saturated control/media
admission; tombstone-only budget progress; and cancellation/lease retirement.
Run affected crate tests, VA-API-feature unit tests without a GPU, Clippy,
formatting and the normal debug distribution build. Re-run the WAN stress test
manually after implementation and assess latency, not merely connection survival.

## September 7 WAN observation and next investigation

The user's `network-hoist-iy5hrjuh` AV1 run reported less catch-up lag but
continued Blender sluggishness. Input delivery versus content cadence was not
isolated. Receiver observations confirm transient backlog, not a proven lack
of hardware decoder throughput:

- Over 109 source samples, encode batch wall time averaged
  22,293,227 / 7,729 = 2,884 microseconds. Receiver completion wall time averaged
  54,986,753 / 7,752 = 7,093 microseconds over its independently sampled span.
  The latter includes worker execution, conversion and host wake/dispatch;
  codec-only and scheduling costs are not yet separated.
- In consecutive receiver intervals ending at 19:49:07 and 19:49:08 UTC,
  159 then 178 media frames arrived, while 125 then 157 completed. Pending media
  rose from zero to 34 and then 55. The next interval received 70, completed 125
  and drained the queue. This establishes temporary receiver-path overload,
  not sustained overload or its cause. Bursty delivery also remains possible.
- Sampled pending media peaked at 55 of 128 records, well below the byte bound.
  Maximum recorded media wait was 819,547 microseconds and control-to-applied
  time was 834,437 microseconds. All 109 source transport snapshots showed zero
  pending records/bytes; maximum recorded write time was 374 microseconds.
  These samples cannot prove that backpressure never briefly engaged.
- A source interval produced 183 layer frames in 1.000091 seconds across four
  streams. Comparing that peak against the whole-run 7.093 ms average suggests
  possible pressure, but does not measure a sustained service-rate deficit or
  prove Blender alone commits above 60 Hz. Codec configuration's 60 fps is not
  a submission timer.

Investigate receiver work distribution first: there is one outstanding decode
per connection and one worker thread, despite separate per-generation decoder
contexts. Each result allocates fresh XRGB output and performs synchronous VPP
conversion before notifying the host. Bounded pipelining, stream-affine worker
concurrency, and safe conversion-resource reuse are candidates; preserve codec
reference order and buffer lifetimes. Neither larger queues nor a per-commit
network ACK addresses that local serialization.

After measuring that path, local latest-state frame pacing and an aggregate
work budget may use periodic asynchronous receiver feedback. Such feedback
would guide admission rather than gate each commit on a reply. No decoder
pool, new feedback protocol or adaptive pacing is included in this commit.
