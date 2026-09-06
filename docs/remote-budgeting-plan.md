# First streaming-budget implementation plan

## Status and scope

The source, receiver, and Iroh transport observations and the first bitrate
actuator below are implemented. Shared budgeting and adaptation remain proposed
work. The actuator precedes the combined feedback/allowance wire change so those
controls have a real consumer. The design has been peer reviewed. The broader
[budgeting specification](spec/remote-budgeting.md) remains Direction, not a
checklist to implement wholesale.

This pass adds shared media budgets, user/network preferences, focus-aware frame
admission, and adaptive bitrate reduction **and recovery**. It does not promise
to fix the GPU hang recorded in [network validation](network-validation.md).
Hardware faults remain fatal diagnostic events, not congestion samples.

### Implemented: initial source observations

`weld-hoist-encoded` now maintains fixed-size interval counters for received and
coalesced commits, completed batches/layer frames and encoded bytes, cancelled
batches, codec failures, applied/cancelled credits, and stale credit replies.
It records total/max completed batch wall time and applied-credit turnaround,
plus gauges for pending events, active logical streams, the active batch, and
outstanding credits/oldest age. This is one source-port aggregate, not a
per-window budget or measured network capacity.

Collection is independent of logging. Set
`RUST_LOG=warn,weld_media_diag=debug,weld_network_diag=debug` on an explicitly
approved validation run to see the summaries. They emit at most once per second
when the port is polled and work occurred or remains, plus a best-effort partial
summary before ordinary teardown. There is no diagnostic timer, forced Bevy
update, idle log stream, or guarantee of a final sample after abort/SIGKILL.
An outstanding stalled credit remains work, even with no completed frames.

Counters describe events, not disjoint outcomes: a cancelled credit may later
produce a stale reply. Batch wall time includes sequential layers and host
completion draining; credit turnaround includes local send queues, network,
receiver processing and host scheduling. Neither is GPU-only time, RTT, or
presentation latency. Encoded-byte counts are produced payloads, not delivered
or carrier-billed bytes. These observations do not change scheduling, codec
parameters, credit policy, or source-buffer lifetime.

### Implemented: receiver observations

The same encoded ports used by Unix and Iroh now collect fixed-size receiver
summaries under `weld_media_diag`. They count accepted commits and media payloads,
submitted/successfully completed decodes, applied encoded commits, lifecycle
cancellations, late cancelled media and codec-result errors. Late cancelled
media is counted separately, not as accepted media or congestion loss. An
obsolete decode can count both a cancellation and a codec error; invalid tokens
and failed results never count as successful decodes. Received commits include
metadata-only commits, while applied/cancelled counters cover commits carrying
encoded replacements and their credit outcomes.

Timestamps occupy existing bounded queue entries. The locally measured stages
are deliberately distinct and **overlap**, so do not add them:

- `media_wait`: media ingress into the encoded port to decode submission,
  including any wait for control metadata and earlier work;
- `decode_wall`: accepted submission to matching successful completion drain,
  including worker processing and host polling;
- `commit_wall`: control ingress through decoded-buffer import and queuing the
  adapter event. This is not actual presentation or acknowledgement delivery.

Ingress here is not socket receipt: time in the binding's receive queue is not
included. Gauges report queued control events, pending media frames/bytes,
decoded frames, logical streams, the active decode, and oldest control/media
and active-decode ages. Queue scans happen only when a summary is due. Idle
mapped streams do not emit continuously; pending work can still report a stall.
Like source observations, collection is independent from tracing and creates
no new timer or compositor wake. A failing decode drain emits a partial summary
before propagating its error, with a best-effort final summary on ordinary Drop.

### Implemented: Iroh send and selected-path observations

Encoded source transports can now provide an optional owned `TransportSnapshot`.
The source consumes it on the existing source-report cadence and clock, without
another host timer, history queue, or compositor wake. The Unix binding currently
reports unavailable; it does not invent QUIC-shaped measurements. Reasoned wire
feedback, receiver allowances, and actual budget enforcement remain subsequent
work. No data-saving or hard-cap option is exposed before it can be enforced.

The Iroh media queue records accepted, fully written, and cancelled record and
payload-byte totals, admission-to-write wait, successful framed-write wall time,
pending record/payload bytes and oldest age, and active-write age. Metadata is
bounded by the channel capacity plus its one active record. A record remains
fully charged until its complete write finishes; these gauges are not counts
of bytes still unsent inside QUIC. Completion means the transport API accepted
the record, not remote receipt, acknowledgement, decode, or presentation. Write
wall time includes framing and task scheduling, not just congestion blocking.
Teardown and failed writes release accounting through record ownership without
classifying cancellation as congestion loss. Unavailable/poisoned measurement
state does not change the existing transport failure or delivery semantics.

One weak-connection observer per Iroh peer now samples selected-path statistics
once per second independently from tracing. It retains only an owned latest
snapshot with local sample time, RTT, congestion window, UDP byte totals, loss
totals, and congestion-event totals. Selection changes, uncertain event history,
and counter rollback change its connection-local epoch; consumers must rebaseline
and check freshness, not subtract totals from different epochs. Missing or closed
paths have no usable snapshot. These counters include connection traffic beyond
media payloads and are not available-bandwidth or carrier-billing estimates.
The opt-in periodic `weld_network_diag` log remains at five seconds; media
summaries include path sample age and epoch on the existing source cadence.
The observer wakes Tokio, not Bevy or the compositor host.

`run-network-hoist` now enables `weld_media_diag=debug` in its default filter and
preserves an explicit `RUST_LOG`. No protocol, codec settings, scheduling,
network-interface handling, or GPU lifetime behavior changes in this sub-batch.
Local transport attribution, coordinated feedback/preferences, and budget
enforcement are still pending. The codec-pool and alpha-atlas
explorations in the specifications do not expand this initial implementation.

### Implemented: generation-based bitrate actuator

Encoded source ports expose optional `EncoderRateControl` handles before adapter
erasure. The concrete Unix and Iroh source-side destination endpoints retain
these weak handles; native-buffer hoisting and custom backends without rate
control return None. Callers can list live stream identities and source
surface/layer mappings, request a rate, and inspect revisioned requested,
submitted, and applied state. No new knobs enter the generic window/client/hoist
protocol. The standard distribution does not yet drive this automatically or
expose a new bitrate/cap flag.

Handles are safe to retain across threads; selection, codec work, and application
remain host-owned. Short mutex sections protect only numeric state, never
spanning backend/transport calls, tracing callbacks, or await. Only live streams
accept requests, duplicate targets reuse revisions, and intermediate requests
coalesce to one latest desired value. Layer/surface retirement removes entries.
Owner closure marks the registry closed and clears it under the same lock as
requests, including callers that upgraded their weak handle before disconnect.
The control retains neither buffers nor encoder contexts.

Every prepared job freezes its bitrate. Later requests cannot rewrite an active
multi-layer batch. A changed bitrate or extent uses the existing single
generation-rotation point after old prepared work completes; changing both
rotates once. Old encoder generations are retired before replacement and never
resubmitted. Existing surface credit still orders receiver processing. A matching
codec packet confirms application; sequence zero must be a keyframe. Applied
means codec output exists, not receiver display or measured wire bitrate.
Failed, rejected, mismatched, and cancelled work never confirms a new rate.
An unexpected frame identity or non-keyframe at generation start is a fatal
backend-contract error, not an advisory confirmation failure.

The first rate change can proceed immediately. Further switches have a two-second
minimum dwell starting at changed-rate submission, not preparation. Requests
apply lazily to the next eligible replacement pixels after dwell. There is no
new timer, synthetic commit, or extra retained frame; static/retained-only
content can keep intent pending. Diagnostics expose requested/applied sums and
pending stream counts when polled. Unusable bookkeeping retains an existing
stream's frozen settings rather than defaulting a lower-rate generation back to
its startup rate. Mandatory budget admission must later treat unavailable
control as unavailable, not assume a cap was enforced.

The VA-API adapter permits lowering and restoring its validated startup bitrate:
AV1 8 Mbps or H.264 16 Mbps per layer. The positive lower bound is numeric
validation, not a usable quality floor or hardware-capacity claim.
`VaapiEncoderSettings::with_bitrate` revalidates overrides while preserving codec,
cadence, and GOP. FFmpeg constructs the replacement with target, minimum, maximum,
and CBR reservoir updated together. No live-context mutation or hot retuning is
assumed. Hardware rate-switch validation remains opt-in and has not run here.

This is the actuator portion of Batch B, not aggregate budgeting or adaptation.
Old-rate work and queued bytes may finish. Immediate admission relief, shared
caps, receiver allowances, and decrease/recovery policy remain pending.

## Verified starting point

- Unix encoded hoisting and Iroh use the same `weld-hoist-encoded` ports and
  `weld-media-vaapi` workers. Defaults are AV1 8 Mbps and H.264 16 Mbps **per
  layer encoder**, not per connection. Both support opaque output only.
- Each encoded source port has one active encode batch, with sequential layer
  encoding, and one outstanding destination credit per client surface. This
  already throttles delivery at higher RTT; reducing bitrate cannot remove a
  stable stop-and-wait cadence limit.
- `EncodedCommitOutcome::Applied` means decoded/imported and queued into the
  client adapter, not displayed. `Dropped` currently originates in surface
  cancellation. It is **not** an existing congestion/drop-rate signal. Several
  queue/resource failures instead terminate the session.
- Source DMA-BUF leases remain held through encode completion, independently
  of receiver acknowledgement. Pending unencoded commits can retain leases
  while awaiting credit and coalesce with newer commits.
- Iroh currently sends all encoded surfaces and layers through **one reliable,
  ordered media stream**, separate from control. A large background access unit
  can delay focused media. Priority can choose what enters that stream next;
  it cannot overtake bytes already submitted to it. Both streams share QUIC's
  congestion budget.
- Encoder settings remain fixed per generation. Rate changes use a replacement
  generation; the worker rejects changed settings within one generation and
  evicts other generations of that stream. Concurrent old/new encoder
  generations cannot be assumed.

Evidence: `weld-hoist-encoded/src/{state,codec}.rs`,
`weld-hoist-iroh/src/{peer,framing,admission}.rs`,
`weld-media-vaapi/src/{worker,ffmpeg}.rs`, and
`weld-hoist-protocol/src/lib.rs`, under `crates/`.

## Ownership and preferences

Keep the policy in one pure budget module in `weld-hoist-encoded`, with a shared
coordinator injected into encoded ports. Do not instantiate a complete budget
independently for every window, layer, GPU, or peer. No new empty framework crate
is needed. Its inputs and outputs contain owned Weld IDs, numbers, durations,
and reason enums, never Bevy, Iroh, FFmpeg, or file-descriptor types.

The coordinator separates local network-scope upload/download pools, per-peer
allowances, and local media-device work/context limits. Multiple ports share
those pools. Each destination owns its receiving/decoding allocation and sends
the source its authorized per-peer allowance; it does not reserve the source's
GPU. Dynamic multi-peer connection admission and a calibrated hardware-capacity
model remain outside this pass. Limits cover this Weld runtime's workload, not
all other applications or independent Weld processes on the machine.

User preferences are inputs to budgeting, not estimates of network capacity:

- Resolve global defaults, a selected network profile, and explicit peer/session
  overrides. Distinguish initial-target defaults from hard aggregate ceilings;
  a session override cannot silently exceed its network-scope hard cap.
- Support local data-saver, balanced, and custom policy selections, explicit
  upload/download media ceilings, initial targets, foreground/background cadence
  caps, and permission to probe upward. Exact preset values are tunable local
  policy, not protocol constants.
- A 5G profile may intentionally be cheaper than a Wi-Fi profile. Metering is
  explicit or supplied by a trusted optional OS hint: Wi-Fi can be metered,
  cellular can be unmetered, and USB tethering looks like Ethernet. Unknown
  remains unknown. Never infer cost from Iroh's IPv4/IPv6/relay path type.
- Start with typed distribution options and CLI overrides. Do not add a wallet,
  NetworkManager dependency to budgeting, persistent configuration framework,
  or SSID-based rule engine merely to select a test profile.
- Send accepted numeric allowances and presentation intent, not SSIDs, IMSIs,
  interface names, or raw network identities. Validate authenticated feedback
  against its peer/session and currently authorized surface/generation.
- The sender respects both its allocated upload cap and the receiver's allocated
  download cap. Neither side may give every connection its entire shared pool.
  User profile changes are revisioned; stale feedback cannot restore an old cap.

The effective target stays below user, receiver, backend, and available local
allocation limits. The controller can discover that less is sustainable; it
cannot decide that a data-saving preference should be exceeded. When a user
lowers a cap, immediately restrict new admission and apply lower codec targets
at a safe boundary. Already-submitted reliable bytes must finish draining:
this is not a promise of instantaneous NIC-rate clipping or a carrier-billed
data quota. Track payload and transport overhead separately.

Window managers provide trusted focus and explicit visibility through the
neutral client/relay boundary. SSD, placeholders, application damage, and client
self-claims do not decide priority. One presentation budget covers a toplevel's
layers and its popups; an independent dialog has its own presentation budget.
Popup responsiveness receives a bounded share within the parent's allocation,
not an extra full bitrate allocation. Unknown visibility is treated as visible.

## Batch A: observations and policy contracts

Make the current bottleneck observable before changing its feedback loop.
Separate a first observation-only sub-batch from enforcing preferences if needed;
do not expose a user-facing hard-cap option until it is actually enforced.

Collect bounded, locally monotonic observations:

- Source demand, admission/coalescing decisions, pending-frame age, encode
  duration, active generations, and actual encoded bytes.
- Connection-wide encoded queue bytes/oldest age, write-blocked time, write
  completion, and credit turnaround. Write completion is not remote receipt.
- Receiver receipt, queue/decode durations, decoded/applied outcome and cause.
  Presentation timing remains unavailable unless a real presentation observer
  is added; do not relabel `Applied` or subtract unsynchronized peer clocks.
- Drop/skip reasons distinguishing cadence policy, hidden state, newer-state
  coalescing, lifecycle retirement, pressure-related lateness, and codec failure.
  Do not reinterpret today's lifecycle `Dropped` records as network pressure.

Use eligible active demand as the denominator for pressure misses. Idle windows,
intentional FPS gating, hidden windows, normal coalescing and retired-generation
traffic are not congestion drops. Preserve exact lifecycle/credit completion
semantics while adding reasoned observations. Feedback and receiver allowances
belong to one coordinated protocol revision change, not separate bumps for each
new record; verify both bindings' actual bootstrap behavior when wiring it.

For the current self-throttling pipeline, **inflated credit turnaround** is a
primary end-to-end pressure observation, even when no frames are dropped. Split
source queuing/write delay and receiver queue/decode time before attributing it
to the link. Stable RTT-limited cadence alone is not congestion. Sustained
unintentional deadline misses/drops, queue growth, and loss provide additional
evidence. Record attribution as unknown when it is not identifiable.

Iroh 1.1.0 provides `Connection::stats()`, selected-path events, and
`Connection::paths()` / `Path::stats()` with RTT, congestion window, lost bytes
and packets, and congestion-event counters. Connection totals include previous
paths. Copy owned scalar snapshots keyed by connection identity, `PathId`, and a
local selection epoch; rebaseline on selection changes and reject stale samples.
Closed-path final statistics are not proof of liveness. QUIC retransmission loss
is not a count of lost video frames. These APIs were checked against the pinned
iroh/noq sources and the [connection documentation][iroh-connection] and
[path-statistics documentation][iroh-path-stats].

Do not build on `congestion_state`, a debugging API, or an optional controller
pacing rate absent on the default controller. Cwnd/RTT and transmitted bytes
are hints, not available-bandwidth estimates. Add an independent bounded observer
with weak connection ownership, not a dependency on the tracing-gated diagnostic
logger. The same observation contract must work without QUIC for local tests.

Batch A also verifies the backend's rate-change capabilities. The spike is
bounded and opt-in for hardware; a negative hot-retune result selects Batch B's
generation replacement, not a permanent "adaptive FPS only" substitute.

## Batch B: a truthful bitrate actuator

Introduce per-stream requested, pending, and applied settings with a change
revision. Report whether a request was applied, needs replacement, or is
unsupported. No unsafe mutation from the host thread while the worker uses a
codec. Recompute the CBR target/min/max and HRD reservoir consistently through
the backend; assigning `AVCodecContext.bit_rate` alone is not proof of retuning.
Use `VaapiEncoderSettings` validation, including the existing AV1 ceiling, rather
than duplicating backend limits in the controller. That ceiling is conservative,
not proof that the current GPU hang is fixed.

Fallback replacement is explicit and serialized:

1. Stop admitting old-generation input and coalesce pending rate requests.
2. Reach a worker-safe boundary: no active encode batch and no old-generation
   `PreparedEncode` still waiting in that batch's layer queue.
3. Retire the old encoder and open its replacement without exceeding shared
   transition/context limits. Never submit old-generation input again; otherwise
   the current worker can recreate it and oscillate between generations.
4. Deliver new configuration and a keyframe in the existing ordered media path.
   Previously encoded old-generation bytes remain valid without their encoder
   context. Preserve decoding order and reject stale-generation re-entry.
5. Activate the new decoded presentation atomically, keeping the previous
   displayed allocation valid until replacement import/GPU consumption permits
   release. Do not assume two decoder generations coexist in the current worker.

Changes are bounded by meaningful rate steps, dwell/cooldown, and one coalesced
pending target—not a context recreation on every controller tick or focus move.
No new pixels means no artificial client commit: apply lazily before the next
eligible input if an immediate replacement frame is unavailable. While a lower
rate is pending, admission supplies immediate relief and reports that the codec
has not applied the target yet. Failure stays visible, with no hidden software
fallback or unbounded retry loop.

## Batch C: shared fixed budgets, priority, and wakeups

Use the actuator to enforce a fixed aggregate media budget before enabling
automatic capacity probing. A conservative 8 Mbps aggregate AV1 test ceiling is
an initial experiment, replacing today's 8 Mbps per layer—not a universal
network default. Explicit user preferences can select a lower cap. Track both
allocated targets and observed output; keyframe/CBR bursts require bounded
headroom, not a claim of an exact instantaneous bitrate ceiling.

Prioritize interactive/focused presentations, then recent focus, then visible
background work, with fair minimum service where feasible. A 60/15 fps
foreground/background split is a tunable starting point. **Cadence admission is
new code**, not a setting on the existing coalescer. Explicitly hidden regular
media can pause while input, cursor feedback, control, lifecycle and reclaim
remain active. Do not build an occlusion engine in this batch.

Apply priority before encoding/enqueueing into the shared media stream. Bound
connection-wide queued bytes and age, including large access units, and account
for transition/keyframe bursts. Never discard arbitrary encoded delta frames
or partially written records. Use latest useful **unencoded** state, preserving
atomic tree replacements, real removals and lifecycle barriers. A permanently
oversized access unit must produce a visible quality/admission decision, not an
infinite token wait. The local binding instead has atomic Unix seqpacket queues
and sealed payload descriptors; its accounting must reflect that shape, not
pretend it has a QUIC byte stream. Control remains outside media admission, but
shared QUIC congestion still prevents an absolute zero-latency control guarantee.

If guaranteed floors cannot fit, use hoist policy to wait, refuse or explicitly
pause/degrade authorized work. Do not overcommit by raising a tiny allocation to
every stream's minimum. Reclaim and failed/disconnected peers release exactly
their reservations. Multiple peer ports must share the network/device pools
even though dynamic multi-peer connection setup is a later feature.

An eligible final pending frame must progress without another client commit.
Own one earliest deadline per coordinator and wake through an adapter-owned
timerfd wrapped by existing `ClientRuntimeWakeSource`. Its `prepare()` drains
expirations because registration is level-triggered. After calloop dispatch,
both native backends already call `ClientRuntime::drain_events`, which polls
the adapters before the Bevy frame gates. Service due work there, rearm to the
next deadline, and disarm when no pending admission or meaningful probe remains.
Do not feed this deadline into the frame-based `dispatch_timeout`, add per-window
threads, or keep Bevy rendering to drive the budget clock.

## Batch D: decrease, hold, and cautiously probe upward

The controller adjusts the peer's shared pool; allocation then subdivides it.
QUIC remains responsible for packet congestion control. Weld controls offered
media work and quality at a slower timescale.

| Observation | Response |
| --- | --- |
| Sustained excess turnaround attributed to the link, queue growth or pressure-related misses/drops | Reduce offered bitrate and frame admission promptly; retain a reason |
| Slow encode/decode with healthy transport | Reduce cadence/work demand first; do not label this a bandwidth estimate |
| Healthy active demand, low queues, fresh feedback after cooldown | Probe a small bitrate increase within all user/receiver/backend caps |
| Idle, intentionally capped, hidden, stale feedback, or protocol-limited demand | Hold; absence of loss does not prove extra capacity |
| Path/profile/allowance change | Rebaseline observations, cancel incompatible probes, apply the new ceiling |
| GPU reset, invalid buffer, codec/protocol failure | Preserve diagnostics and fail/recover through existing ownership; do not treat as congestion |

A starting experiment is one-second decision windows, a roughly 20% reduction
after two bad windows, and a small roughly 5% upward probe after several healthy
windows plus cooldown. These are **unvalidated tuning seeds**, not wire rules.
Minimum sample requirements and queue-age evidence must support sparse streams
without inventing samples for idle time. Severe bounded-queue pressure can stop
new admission immediately. Use hysteresis, bounded steps, and rollback to the
last sustainable target if a probe makes pressure return.

Only probe when unmet real demand can exercise the increase. If actual media
load does not rise, the probe did not establish capacity. Do not generate dummy
traffic or increase indefinitely on a static terminal. A disabled upward-probe
preference is respected. Downward and upward changes must request actual codec
bitrate updates through Batch B; frame throttling alone is not completion of
this feature. Avoid repeated reactions to old samples while settings are pending.

## Batch E: bounded validation and acceptance

First use deterministic fake clocks, transports and codecs. Cover:

- sustained bandwidth fall, stable constrained operation, recovery probing and
  rollback; packet loss without lateness; stable high RTT without congestion;
- decoder/encoder overload separately from write/credit delay; stale, duplicate,
  malformed, wrong-session and wrong-generation feedback;
- idle/static demand, intentional skips versus deadline misses, and the last
  pending frame waking without another client update;
- focus flapping, popup/dialog grouping, fair service, hidden/resumed streams,
  impossible quality floors, and multiple ports sharing caps;
- user/profile precedence, metered/unmetered/unknown hints, lower receiver caps,
  preference changes during a probe and all probes respecting hard limits;
- resize, unmap, cancel and peer loss during a rate switch; delayed old packets,
  no old encoder re-entry, bounded generations and correct lease retirement;
- keyframes larger than the normal allowance, actual-versus-target bitrate,
  bounded queue memory, and continuing control/input under media pressure.

An opt-in, bounded application-level impairment harness should apply the same
delay/bandwidth scenarios to local AV1 and Iroh without changing host routes,
interfaces or `tc` rules. Do not drop framed reliable bytes to simulate frame
loss; delay delivery or inject reasoned outcomes through the fake contract.

Hardware validation follows the actuator and lifetime tests, with explicit
approval, short runs and immediate stop on a GPU fault. Compare the same app,
codec, extents, window count and scripted interactions. Capture bounded aggregate
metrics and first failure, not raw video/input dumps. Require aggregate budget
compliance within the declared burst/transition envelope, fewer pressure misses,
recovery within the allowed ceiling, responsive focused work, and bounded leases,
memory and generations. The existing VCN hang remains an independent open issue.

## Deferred work and implementation boundary

No resolution adaptation, atlas/composed-stream redesign, alpha/stereo/foveation,
deadline datagrams, multi-frame credit pipeline, hardware calibration, global
cross-process GPU allocator, data-quota billing, or phone UI in this pass.
The reliable-stream and stop-and-wait limitations stay visible rather than being
hidden by ever-larger queues or bitrate changes.

Expected changes stay in `weld-hoist-encoded`, `weld-hoist-protocol`, binding
observation adapters, and `weld-media-vaapi` for rate actuation, with small
distribution/client-window observation bridges when first consumed. No budget
policy belongs in SSD, Iroh internals, or Smithay. Batches A through E each need
their own implementation checks/review and coherent commits. Source-level checks
must confirm exact APIs at implementation time; this plan does not authorize
speculative placeholders for deferred features.

[iroh-connection]: https://docs.rs/iroh/1.1.0/iroh/endpoint/struct.Connection.html
[iroh-path-stats]: https://docs.rs/iroh/1.1.0/iroh/endpoint/struct.PathStats.html
