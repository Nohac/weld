# Remote media budgeting and prioritization

## Boundary and status — Direction

Remote media shares finite encoder, decoder, conversion, memory, network, CPU,
power, and thermal resources across presentations. One budgeting authority per
physical media device owns admission estimates, live reservations, priority,
quality tiers, and scheduling recommendations. A window, codec adapter, or
transport binding does not independently reserve hardware.

This authority consumes, but does not define, window state. Source focus,
workspace membership, z-order, and visibility come from the
[window-management
model](window-management.md#composable-responsibilities--direction). Trusted
focus and input delivery come from
[Surfaces and input](surfaces-and-input.md#seats-and-devices--direction).
Destination active-workspace presence, visible fraction, and occlusion come
from authorized
[presentation-target
observations](remote-presentation.md#presentation-target-model--direction).
Clients cannot declare themselves focused, visible, occluded, or high priority.

Budgeting also does not own wire capability records, codec path mechanics,
stream generations, or media topology. Those remain in the
[remote protocol](remote-protocol.md). It recommends admission, reservations,
quality changes, pauses, and renegotiation; hoist policy accepts, refuses, or
asks the user under
[Media admission and
degradation](remote-hoisting.md#media-admission-and-degradation--direction).

No budgeting implementation or calibrated hardware model exists yet. The
rules below constrain the first tracer without promising exact limits before
measurement.

The next bounded implementation sequence is recorded in the
[streaming-budget plan](../remote-budgeting-plan.md), separate from this broader
Direction material.

Each peer runs budgeting for its own physical media devices. Under the
[complete-path
ownership](remote-protocol.md#complete-path-selection--direction),
the source reserves import, conversion, and encode resources while the
destination reserves decode, presentation, software CPU, power, and thermal
resources. Neither peer reserves hardware owned by the other. Stable stream
identity correlates their local reservations, and the
[offer and answer](remote-protocol.md#hoist-offer-and-answer--direction)
activates a presentation only after required local admissions succeed.
Transport observations remain a budget dimension accounted for by peer-local
policy; this document does not invent a third link scheduler.

## Independent resource dimensions — Direction

The scheduler tracks independent constraints rather than one percentage:

- hard, reported, or hinted encoder and decoder context counts;
- codec, profile, bit-depth, alpha, extent, and cadence performance points;
- aggregate aligned pixels or coding blocks per second;
- GPU conversion and memory bandwidth;
- GPU memory for conversion, reference, and presentation surfaces;
- encoded bitrate and binding congestion;
- destination decoder, presentation, CPU, battery, and thermal capacity; and
- transient headroom for codec or resize generation replacement.

The [extent
model](remote-presentation.md#extent-and-performance-envelopes--direction)
already distinguishes hard dimensions from aggregate pixel rate and
concurrent sessions. Budgeting turns those facts into reservations; it does
not reduce them to a portable claim that a GPU supports a fixed stream count.

Lower extent or cadence often permits more streams when aggregate throughput
is limiting. It may not help when firmware, licensing, driver, memory, or hard
context limits dominate. Codec and profile costs also differ. A scheduling
estimate may begin with:

```text
aligned pixels * cadence * codec cost * bit-depth cost * color/alpha planes
```

That is a local estimate corrected by evidence, not a protocol guarantee or a
linear hardware law. Hard context ceilings remain distinct even when estimated
pixel-rate capacity is available.

## Discovery and calibration — Direction

Platform-reported instance hints, supported sizes and rates, performance
points, alignment, and memory formats seed the model. Such reports are upper
bounds or evidence, not a promise that every combination remains simultaneously
available while other applications use the device.

Opaque backends require opt-in calibration over representative codec, extent,
cadence, conversion, and concurrency combinations. Calibration must not
silently create many hardware contexts or stress a live desktop. A normal
session begins conservatively when no calibration exists and refines estimates
from actual operation.

The budgeting authority owns live and persisted calibration records
semantically. Records are keyed by physical media device, driver and relevant
system version, codec backend and version, codec/profile, and measurement
method. A future storage adapter may persist this project-owned data.
Configuration selects policy but does not own observations. Driver, backend,
device, or schema changes invalidate incompatible records.

Runtime encode/decode latency, queue depth, missed deadlines, hardware resource
errors, memory pressure, throughput, loss, power, and thermal observations
correct the estimate. A successful context creation does not prove sustainable
cadence; sustained deadlines matter.

## Codec pools and auxiliary capacity — Exploration

Hardware engines, concurrent codec sessions, sustainable pixel throughput, and
memory are separate limits. A Weld safety bound on live generations is not a
measurement of the device's physical encoder count. Lower resolution or cadence
can relieve throughput pressure without increasing a hard session ceiling.

Explore a device-local capacity pool with dedicated interactive/high-detail
streams, auxiliary capacity for popup color and alpha masks, lower-cadence
background atlases, and unallocated transition/external-work headroom. These
are allocation roles, not fixed portable numbers of hardware encoders. Reserving
"all but one" context is only a possible policy seed: leave throughput and
memory headroom too, and do not promise another application an OS-enforced
reservation. Encoder and destination decoder budgets must both admit a layout.

Reserve capacity in bookkeeping before allocating actual sessions. Start
sessions and appropriately sized buffers lazily; consider only a small bounded
warm pool if startup measurements justify it. Do not discover the device limit
by opening every possible encoder during normal startup, or allocate every
session at the device's maximum extent.

Auxiliary grouping should consider observed color and alpha change cadence,
trusted focus/attention, visibility, quality guarantees, and recipient
compatibility. Frequently changing alpha can share an active group, while static
masks remain retained without repeated transmission. A focused presentation's
required alpha gets corresponding service; alpha is not automatically an
optional enhancement that can be dropped under pressure. Pins, accessibility,
and other admitted guarantees still constrain focus-based policy.

Sharing one auxiliary pool does not require one universal auxiliary stream.
Mixing a rapidly changing popup with many static masks can force work over the
whole atlas at the popup's cadence. Group by compatible cadence, use stable
placements, and apply dwell/hysteresis to promotion and regrouping rather than
repacking on every focus or damage event. Charge composition, aligned coded
pixels, memory, contexts, and destination decoding as well as encoded bytes;
cheap compression of flat masks is not proof of cheap hardware processing.

Packing may reduce session count; downscaling and lower cadence may reduce
pixel work. Neither is a substitute for measuring the limiting resource. The
protocol owns the [auxiliary layout and alpha contracts][auxiliary-alpha], while
budgeting recommends groups and allocations. This exploration is deferred from
the first fixed/adaptive network-budget implementation.

[auxiliary-alpha]: remote-protocol.md#auxiliary-color-and-alpha-atlases--exploration

## Reservation model — Direction

Each active presentation stream has a peer-local reservation containing at
least:

- stable stream, peer, and presentation-target identities;
- selected local capability and remote counterpart capability identities;
- codec profile and acceleration at both endpoints;
- aligned capacity extent and current visible extent;
- target and observed cadence;
- application view count and packed or separate-view layout;
- color and alpha plane count;
- locally allocated encoder or decoder context count;
- local conversion, presentation, and in-flight-frame count;
- estimated pixel rate and GPU memory;
- reserved and observed encoded bitrate;
- admitted quality floor and current quality tier; and
- temporary generation-migration headroom.

Reported hard limits, calibrated sustainable estimates, live reservations, and
observed use remain separately inspectable. A failed reservation cannot be
hidden by editing the estimate after admission.

Source and destination reservations for one stream are reconciled by the
protocol but remain independently owned and released. One logical presentation
visible on several targets may share work only when
their codec, geometry, cadence, color, and quality requirements are compatible.
Otherwise the scheduler requests separate renditions and accounts for each one.
It does not duplicate the highest-cost rendition blindly.

Paired alpha is explicit capacity. Budgeting reserves every plane, codec
session, pixel-rate contribution, memory allocation, and bitrate selected by
the protocol's
[alpha path](remote-protocol.md#adaptive-and-alpha-media--direction) rather
than inferring one stream from the presentation identity.

Multiview content is likewise explicit capacity. A packed stereo access unit
may use one codec context, but budgeting charges the combined aligned pixels,
conversion bandwidth, decoded storage, destination sampling, and observed
bitrate. Separate eye streams additionally reserve their real encoder and
decoder contexts. Repeated mono/stereo toggling is subject to the same
generation-migration headroom and hysteresis as resize.

Fixed stereo may share one rendition among compatible targets. Head-tracked
content generally cannot: each independently moving viewer contributes its own
pose-driven rendition, predicted deadline, aligned view pixels, encode/decode
work, and in-flight surfaces. Admission must expose that multiplier rather than
silently degrading every viewer when another headset joins.

Alpha and depth are separate budgeted planes even when they belong to one
spatial frame group. Policy may lower depth resolution or cadence when
reprojection remains useful, but it must preserve explicit synchronization and
the admitted color and alpha floors. Pose-to-photon deadlines take precedence
over draining obsolete work; a late tracked frame is dropped or reprojected
according to negotiated policy instead of building a FIFO backlog.

## Conversion surfaces and memory — Direction

The remote protocol owns the
[direct DMA-BUF versus GPU-conversion
path](remote-protocol.md#buffer-frame-and-queue-lifetimes--direction) and
bounded media queues. Budgeting owns only the number, extent, and memory
reservation of conversion surfaces required by the selected path.

A compatible direct client DMA-BUF needs no Weld-owned encoder-input ring. A
crop, composition, effect, alpha, format, scale, or synchronization conversion
allocates encoder-compatible surfaces lazily at the selected aligned session
extent, not the hardware maximum. Codec reference and reconstruction surfaces
remain backend-owned but contribute reported or measured memory cost.

Tightly packed 8-bit NV12 is approximately `1.5 * width * height` bytes per
surface: roughly 3 MB at 1080p and 12 MB at 4K. Typical 10-bit formats stored in
16-bit components can approach twice that. Modifier padding, alignment,
multiplane allocation, reference frames, alpha, and driver-private memory add
cost, so these figures explain scaling rather than promise allocation size.

The first low-latency probe may begin with two to four conversion surfaces.
That is an Exploration seed, not a wire constant. The backend should report or
measure its in-flight requirement, and budgeting reserves that requirement plus
only the demonstrated pipeline slack. Encoded payload storage is smaller,
separately owned, and independently bounded.

## Priority classes and observations — Direction

Priority is lexicographic before it is numerical. The default classes, highest
first, are:

1. **Guaranteed or pinned** — an explicit user, accessibility, recording,
   game, call, or distribution policy promises a quality floor.
2. **Interactive** — the presentation is receiving current trusted interaction
   on an active target. Focus alone receives a modest baseline boost, not this
   maximum class. Clicks, scrolling, keyboard input and active drags promote
   immediately; focused pointer movement promotes rapidly based on elapsed
   movement time rather than device event frequency. Boosts decay when idle.
3. **Recent** — it was focused recently and remains part of the likely working
   set.
4. **Visible background** — it is visibly presented but not currently focused.
5. **Retained** — it needs only a thumbnail, preview, or infrequent refresh.
6. **Suspended** — it is absent from every active target workspace, fully
   occluded, destination-hidden, or explicitly paused.

Within a class, policy may consider visible fraction, damage and requested
cadence, target foreground state, explicit user or application policy, current
resource cost, and time waiting for service. These modifiers cannot elevate a
client past an explicit authorization or quality policy.

The scheduler records both last-focused time and accumulated focus duration for
the current session. Recent focus decays instead of dropping immediately on one
focus transition. Accumulated focus helps identify the active working set but
also decays or is bounded so an old long-running focus cannot dominate forever.
Exact decay curves remain distribution policy and require measurement.

Input immediately promotes its authorized target before waiting for a later
media frame. The compositor or destination supplies focus and visibility
observations; application damage alone cannot claim interaction priority.

An XR destination may additionally report coarse gaze-derived attention for an
authorized presentation. This is a trusted destination observation, not client
self-promotion and not necessarily keyboard focus. Raw eye-tracking coordinates
remain destination-local by default; budgeting needs only presentation
identity, strength, and recency. Dwell, hysteresis, and recent-attention grace
prevent quality oscillation. Initial policy prioritizes whole presentations;
within-window foveated regions require an explicitly negotiated media layout.

When that layout is available, the budget separates an always-usable full-view
base from gaze-local enhancement regions. The base owns the admitted coverage
and recovery floor. Enhancements compete for remaining bitrate, encoder and
decoder contexts, aligned pixel rate, composition work, and latency headroom.
Policy sizes a guard band using observed eye-tracking, transport, decode, and
presentation delay rather than assuming the reported gaze point will remain
stationary. Under pressure it reduces or drops enhancement detail before
violating the base floor.

The default spatial-workspace policy is hierarchical rather than giving every
window an equal full-resolution stream. Visible background presentations receive
only their admitted low-detail base and may run at reduced cadence. The focused
and gaze-confirmed presentation is promoted to a higher whole-window bitrate
and is normally the only presentation reserving a high-resolution foveal
enhancement. Focus changes transfer that reservation with hysteresis and a
bounded overlap period. This makes aggregate source work scale roughly with the
background bases plus one interactive enhancement, rather than the number of
open windows multiplied by full-detail stereo.

The savings include source-side composition or downscaling, pixel conversion,
encode pixel rate, encoded bitrate, queue memory, and network throughput. They
include client rendering only when policy also negotiates a smaller application
raster or the application performs foveated rendering. The measurements report
those categories separately so a cheap media path cannot conceal an application
that continues rendering every pixel at full cost.

A pair of low-detail eye views plus a pair of high-detail gaze fragments may be
cheaper in bandwidth than full-detail stereo, but it is not automatically
cheaper in hardware sessions or decode power. Codec-native ROI or quantization
maps may preserve one context; separate enhancement streams may require several.
Admission uses probed endpoint costs and never infers savings from the word
"foveated." Local native applications do not reserve streaming media merely
because their XR runtime uses foveated rendering.

## Workspaces, visibility, and occlusion — Direction

A trusted, sufficiently precise destination observation that a presentation is
absent from every active target workspace stops regular video while control,
lifecycle, authorization, and reclaim continue. Reentry requests a recoverable
frame or keyframe before revealing stale content. An optional retained
thumbnail is a separate low-priority reservation. Missing, disabled, or coarse
workspace observations conservatively treat the presentation as possibly
present and cannot suspend it.

A trusted, sufficiently precise fully occluded observation likewise stops
regular media. Partial occlusion may reduce extent, cadence, or region quality
only when the destination supplies precise visibility information and the
chosen media layout can exploit it. Missing, disabled, or coarse occlusion is
treated as visible. Occlusion never destroys the window or removes it from the
admitted hoist.

A single-active phone target gives its active presentation the primary budget.
Other windows are retained, suspended, or represented by low-cadence
thumbnails. Switching the active item promotes it and resumes through the
protocol's recovery mechanism.

Desktop and workspace targets may demand several simultaneous presentations.
The scheduler evaluates the union of active targets. A window visible on one
target is not suspended merely because another target hides its workspace.
Conflicting requirements may produce separate negotiated renditions rather
than one accidental highest-resolution stream for every consumer.

Visibility and workspace changes trigger scheduling work directly. The
scheduler does not poll ECS state to rediscover them.

## Quality tiers and degradation order — Direction

When demand exceeds capacity, default policy releases the least valuable work
before reducing interactive quality:

1. Stop regular media for explicitly suspended or destination-hidden
   presentations and those confirmed off-workspace or fully occluded.
2. Reduce or remove retained thumbnails and preview refreshes.
3. Reduce recent presentations after their focus grace expires.
4. Reduce visible-background cadence, then visible extent or bitrate.
5. Preserve the admitted floor for interactive and guaranteed presentations.
6. Recommend codec, acceleration, rendition, or media-topology renegotiation
   through the remote protocol.
7. Queue, refuse, or ask the user before violating an admitted quality floor.

This order produces recommendations only. Any result below the admitted alpha,
color, hardware, extent, cadence, or quality contract returns to the owning
[hoist-admission
rule](remote-hoisting.md#media-admission-and-degradation--direction) for
disclosure, approval, or refusal.

Network bitrate fairness and media-device fairness are separate budgets. A
stream may be cheap to encode but expensive on the link, or the inverse. The
transport reports congestion; budgeting chooses per-stream recommendations;
the protocol performs any resulting profile or rendition change.

## User data preferences and adaptive recovery — Direction

User policy limits what adaptation may spend. Resolve defaults, a selected
network profile and explicit peer/session preferences without allowing a local
override or remote report to silently exceed a hard aggregate upload/download
ceiling. Receiver allowances and sender limits both apply, and connections share
their local network-scope allowance rather than each receiving its full value.
Those pools are distinct from per-device encode/decode work limits.

Users may choose a lower ceiling for mobile data than for home Wi-Fi. Metering
is explicit or comes from a trusted optional OS hint; neither Wi-Fi nor cellular
implies a particular billing policy, and Iroh IP/relay paths do not identify the
access technology. Unknown stays unknown. Keep network identities and profile
selection local; communicate accepted numeric limits and necessary intent.
Media bitrate accounting does not equal the carrier's billed bytes or a monthly
quota, because retransmissions, transport overhead and unrelated traffic differ.

Sustained pressure-related deadline misses or dropped work should reduce offered
bitrate. Count reasons: normal coalescing, deliberate cadence reduction, hidden
presentations, idle time and lifecycle cancellation are not congestion drops.
In a credit-limited path, inflated turnaround and queue age may expose pressure
before drops occur. Separate link pressure from slow encode/decode and from a
stable protocol-imposed cadence limit.

Recovery probes increase quality cautiously after healthy, fresh observations
and cooldown, only when real demand can exercise the increase. Hold on idle or
stale evidence, roll back harmful probes, and rebaseline on path or preference
changes. Never probe beyond the user's ceiling or receiver/backend allowance.
An explicit preference can disable upward probing. Requested and applied codec
rates remain separately observable; reduced cadence alone is not proof of an
applied bitrate reduction. Quantitative thresholds require measurement.

## Fairness, recency, and stability — Direction

Focus is a baseline attention signal, not maximum priority or the only entitlement. Explicit calls,
audio-linked visuals, games, recording, accessibility, user pins, and guaranteed
background work can retain a floor without focus. These policies are visible
and revocable rather than inferred from an application identity.

Eligible visible streams receive bounded minimum service when capacity allows.
Priority aging prevents a continuously focused stream from permanently starving
all visible peers. A recent-focus grace interval avoids an abrupt quality cliff
when the user alternates between windows.

Hysteresis prevents small load, focus, occlusion, or bandwidth changes from
flapping quality tiers. Promotions needed for current interaction happen
quickly; expensive demotions, context destruction, and profile changes require
a meaningful sustained condition or explicit transaction boundary.

When no allocation can satisfy all guaranteed floors, budgeting reports the
conflict instead of inventing precedence. User or distribution policy chooses
which guarantee to reduce, pause, move, or refuse.

## Encoder extent allocation — Direction

An encoder session normally has one fixed coded capacity extent. The initial
policy allocates the smallest extent that covers the settled visible stream
after codec and backend alignment. It does not allocate every stream at the
device maximum, because unused coded pixels still consume processing, memory,
and concurrency budget even when they compress cheaply.

Premature standard 720p, 1080p, 1440p, and 4K buckets are not required.
Arbitrary windows have arbitrary aspect ratios, and exact aligned extents waste
less steady-state work. If measurements show context creation or allocation is
expensive, modest reusable capacity classes may be introduced without changing
the reservation model.

Capacity grows when the required visible extent no longer fits. It shrinks only
after a resize transaction completes or meaningful underuse persists, avoiding
recreation for alignment noise. The current capacity, visible extent, padding,
and temporary scale remain observable so quality UI explains the result.

Cadence can often fall simply by submitting fewer frames. Bitrate retuning is
backend-specific. A coded extent, codec, profile, or incompatible format change
requests a new protocol stream generation rather than mutating a generation in
place invisibly.

## Resize scheduling — Direction

Interactive resize is a transaction, not a stream of encoder allocations.
While an explicit CSD or SSD resize session is active, budgeting retains the
current generation and requests temporary destination scaling or correctly
mapped padding. It does not recreate the encoder for every client commit.

At interaction end, budgeting selects the next aligned extent and requests the
existing
[generation-switch
mechanism](remote-protocol.md#runtime-adaptation-and-renegotiation--direction).
The protocol owns configuration, keyframe, activation, and retirement ordering.
If spare context capacity exists, budgeting may reserve old and replacement
generations concurrently. Without that headroom, it releases the old context
and reports a bounded frozen or scaled interval while the replacement starts.

Target orientation and explicit layout changes provide their own transaction
boundaries. For application-initiated size changes with no interaction end,
commits schedule a bounded stabilization decision after size changes; they do
not start a periodic poll. Persistent growth can force an earlier capacity
increase, while shrink waits for stability.

Resize overlap, context creation latency, visible stutter, and temporary memory
and session cost are measured separately. An implementation must not assume
seamless overlap on hardware whose hard context count is already exhausted.

## Desktop scheduling and topology requests — Direction

Desktop policy directs most media capacity toward focused and recently focused
presentations, then toward visible background work. It suspends presentations
confirmed outside every active virtual workspace and those confirmed fully
occluded on all targets. Non-focused visible windows normally lose cadence
before resolution so their static content remains legible, subject to measured
codec and link behavior.

The scheduler may recommend a different media topology when a hard context
limit, rather than aggregate throughput, is the measured bottleneck. The remote
protocol remains the sole authority for
[atlas and stream topology][protocol-stream-topology],
including the rule against rapidly juggling unrelated surfaces through one
stateful encoder. Budgeting supplies cost and demand evidence; it does not
construct atlas coordinates.

User pins and explicit guarantees override focus-only policy. A game, call,
recording, accessibility view, monitoring panel, or audio-linked presentation
may therefore outrank an ordinary focused terminal according to visible policy.

## Admission, failure, and observability — Direction

For a requested presentation, budgeting returns one of: admit at the requested
floor, admit at an approved lower tier, wait for capacity, pause existing work
under policy, or refuse. Hoist policy remains the authority that applies the
result and obtains required user approval.

Resource creation failure, missed deadlines, or sustained queue growth updates
the measured model and triggers a new recommendation. Protocol renegotiation
and hoist admission remain responsible for applying it. Peer loss releases its
live reservations without discarding calibration evidence.

Diagnostics and user interfaces should expose:

- physical media device and backend;
- reported versus measured context and performance limits;
- active reservations and temporary migration headroom;
- estimated versus observed pixel rate, memory, and bitrate;
- per-presentation priority class, quality tier, and reason;
- current focus recency, visibility, workspace, and guarantee inputs;
- encode/decode latency, queue depth, drops, and missed deadlines; and
- each active reduction, suspension, refusal, or pending renegotiation.

User-facing summaries avoid false precision. “Three of four reported contexts
are reserved” and “estimated encoder capacity is saturated by 4K60” are more
honest than one unexplained utilization percentage.

## Initial measurement matrix — Exploration

The first calibration and tracer matrix varies:

- representative Intel, AMD, NVIDIA, Android, and other mobile hardware;
- AV1, VP9, and H.264 where the backend truthfully supports them;
- direct client import and GPU conversion or composition;
- 720p, 1080p, 1440p, and 4K reference extents plus arbitrary window aspects;
- 15, 30, 60, and 120 Hz where supported;
- one through failure or unsustainable concurrent sessions;
- opaque and paired-alpha reservations;
- focused, visible-background, occluded, and off-workspace demand;
- old/new generation overlap and no-headroom replacement; and
- interactive and application-initiated resize.

Record context creation time, sustainable cadence, p50 and p95 encode/decode
latency, queue growth, drops, pixel rate, memory, bitrate, power and thermal
state, failure onset, recovery, and effect on unrelated desktop presentation.
The matrix must distinguish outright context failure from a configuration that
creates successfully but cannot sustain deadlines.

Measurements establish policy envelopes for the tested device and software
stack. They do not become universal vendor promises.

## Open work — Exploration

- Select the smallest reservation and recommendation interfaces that hide
  backend-specific session arithmetic.
- Define calibration storage, invalidation, export, and privacy details.
- Calibrate codec and profile cost factors without treating them as linear.
- Determine focus-recency decay, grace, aging, and minimum-service policy from
  realistic desktop use.
- Validate full and partial occlusion observations across destination layouts.
- Measure gaze-driven XR prioritization, transition hysteresis, and peripheral
  quality floors without transmitting raw gaze coordinates.
- Measure fixed-view sharing versus per-viewer tracked renditions, including
  pose-to-photon deadlines and color/alpha/depth plane costs.
- Compare full-detail stereo against base-plus-enhancement and codec-native ROI
  paths across bitrate, session count, decode power, guard-band size, and visible
  quality during rapid saccades.
- Decide when several compatible targets share one rendition or require more.
- Measure exact aligned extents against reusable capacity classes.
- Establish policy for conflicting user-pinned or guaranteed quality floors.
- Define atomic reconciliation and rollback when source and destination local
  admission results change concurrently.
- Validate the shared peer-local coordinator's network-scope pools and distinct
  media-device limits across multiple ports; do not replicate a full link budget
  inside each device or connection. Cross-process coordination remains open.

[protocol-stream-topology]: remote-protocol.md#surface-graph-and-media-stream-topology--direction
