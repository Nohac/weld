# Shared encoder targets

## Implemented scope

The standard distribution enables one shared encoder target for its encoded
source, over either Unix or Iroh: **8 Mbps for AV1, 16 Mbps for H.264**. This is
default-on for development testing by explicit choice, not a discovered network
capacity or universal network recommendation. Native-buffer hoisting is unchanged.

Override the source target with `--hoist-bitrate-target-mbps MBPS`, or use the
environment variable inherited by the validation launchers:

```text
WELD_HOIST_BITRATE_TARGET_MBPS=4 scripts/run-local-hoist --codec av1
WELD_HOIST_BITRATE_TARGET_MBPS=4 scripts/run-iroh-hoist --codec av1
```

The precedence is explicit flag, environment, codec default. Values are positive
integer decimal megabits per second. An invalid source value fails before GPU or
network startup. Destinations and native sources ignore the inherited variable;
an explicit bitrate flag on a non-encoded-source invocation is rejected. Startup
logs include the selected target and its origin.

This is an **encoder target, not a bandwidth cap**. The allocator bounds the sum
of its desired targets. Old prepared work, the two-second rate-switch dwell,
static content, queued reliable bytes, CBR/keyframe bursts and protocol overhead
mean applied targets and actual traffic can exceed that sum. No network-capacity
adaptation, receiver allowance, FPS admission or carrier data quota is added here.

## Allocation and churn

Activity-weighted presentation groups share the target. Each toplevel's layers
and owned popups form one group; independently managed dialogs form another. Groups
are scoped by source-port membership and hoist session. Layers divide their group
by full input-buffer pixel area, not logical/cropped window geometry. Codec
padding is not exposed here and is not included in that estimate.

Presenters can explicitly pool independent windows with a
`SetBitratePreference` group label and primary, companion or utility role. The
label is scoped by source-port membership and the actual namespaced `ClientId`,
not app-ID/title strings; it can span that client's hoist sessions. The group
gets one entitlement using its strongest member attention, then divides by
buffer area times the policy's role weights (default **4:2:1**). Switching input
between members does not change that internal split. Popups inherit their
validated window root's preference. Clearing the hint restores ordinary window
allocation; withdrawal or session replacement retires it. No hint leaves the
original allocation unchanged. Input focus, scheduling groups and window
parentage do not change.

Pooling intentionally removes the advantage of opening several background
windows, so an unfocused application can receive less than when its windows
competed separately. These are weights, not readability guarantees or new
minimum bitrate reservations. Existing codec limits and target-switch behavior
still apply. The per-window diagnostic includes the resolved allocation group
and role alongside requested/applied targets.

Both allocation levels reserve backend minima and redistribute capped shares.
The allocator leaves 5% spare headroom where minima permit, rounds ideal rates
down to 64,000 bit/s steps, then avoids small optional changes. With a single
layer, the initial desired rates are therefore **7.552 Mbps AV1** and
**15.168 Mbps H.264**.

Existing targets are reduced when needed to fit the configured total or make
room for meaningful increases in other streams, including interaction boosts.
Donors with the largest excess over their ideal are reduced first, with port and
stream IDs breaking ties. Each selected donor moves fully to its ideal: it pays
for an encoder switch anyway, and this restores room for later small streams.
Optional increases need at least the larger of
64,000 bit/s and 25% of the current target, and must fit. Tiny popups can use
spare headroom without restarting their parent's encoder. Larger layout changes
can still cause several encoder replacements/keyframes; there is no global
staggering guarantee. Below-step ideals for custom backends remain exact rather
than collapsing to their minimum, but their small increases can remain unapplied.

These rules deliberately preserve some unequal shares. A stream can remain
below its ideal by less than the increase threshold indefinitely; small surplus
shares can also persist while the total fits. Idle/static windows retain shares
even while producing no traffic. A larger target can approximate the old
per-layer behavior, while each encoder remains bounded by its backend maximum.

The allocator reuses the scheduler's trusted input recognition and popup grouping,
but quality changes use a longer hold than queue priority. The default
`BitrateAllocationPolicy` weights are background **1**, settled focus **2**,
initial motion **2**, and interaction **12**. Discrete input and sustained motion
retain their boost for **10 seconds** after the last qualifying input. Real input
can promote immediately; focus alone settles for **500 ms**. During a focus-only
handoff the old bonus remains until the new one settles, so rapid refocusing does
not create intermediate encoder switches. Returning to the old group cancels
the handoff; explicit focus clear or unmapping clears that focus bonus immediately.
Initial motion does not bypass focus settling. Sustained motion uses the existing
scheduler's dwell recognition, not a second quality-specific motion timer.

Recent interaction can leave several groups boosted temporarily. This is
intentional to avoid oscillating quality between input bursts; the ratios are
weights on capacity remaining after minima, not promised percentages or calibrated
readability guarantees. A single group still receives the same total allocation
regardless of its class. Each source port has independent focus observations.

Input timestamps extend holds without reallocating while effective classes remain
unchanged. Expiry is checked across participating ports on ordinary admission,
completed input batches and budget operations, including for idle peers. No timer,
synthetic frame or new wire event is introduced. The existing two-second encoder
switch dwell can delay applying a new target; requested priority is not proof
that the next displayed frame already uses that quality. Background FPS reduction,
per-application ceilings, fullscreen suspension and congestion adaptation remain
future work.

## Ownership and failure boundaries

`weld-hoist-encoded::SharedBitrateBudget` is host-thread-owned. Custom
distributions inject clones of one handle into all participating source ports;
constructing a separate budget for every peer would multiply the total. Generic
constructors and registration options still allow no budget. No new empty
framework crate, Bevy resource or transport-specific policy is introduced.

Membership retains numeric inventory and weak rate-control handles, not codecs
or buffers. Never-reused port IDs scope stream IDs. Drop releases that port's
reservations; if the coordinator is borrowed, a dirty flag and weak liveness
token guarantee pruning on the next plain admission/update/snapshot, even with
unchanged inventory. Target publication occurs outside coordinator borrows and
only writes numeric actuator state. The actuator remains `Send + Sync`; source
ports containing the optional host-owned coordinator are no longer `Send`.

A failed actuator publication removes that member and reallocates surviving
ports, instead of making every peer fail. The original error is logged; the
removed source fails its next ordinary admission/update rather than continuing
at a stale fallback rate. Already-prepared jobs remain frozen. Allocation errors
such as impossible minima do not evict members. Registry retirement and inventory
publication must stay adjacent, with no intervening budget operation.

Manual requests through any `EncoderRateControl` handle are rejected once a
budget owns that registry. Requested/submitted/applied inspection remains usable.
Before freezing any frame, the source preflights minimum reservations, registers
the complete scheduled layer inventory, and publishes its targets. Prepared
multi-layer jobs retain frozen rates. Real replacements, not synthetic commits,
apply changes through the existing serialized generation switch.

The VA adapter advertises a **provisional 128,000 bit/s control floor**, not a
probed hardware minimum or quality guarantee. This avoids tiny popup allocations
with byte-sized CBR reservoirs. General `VaapiEncoderSettings` callers remain
free to experiment below it. A large layer near this floor can look very poor;
its 16 KB one-second reservoir and driver/keyframe behavior are not validated.

The current 16-generation bound is per encode worker/source port, not global.
Its minimum reservations total 2.048 Mbps for 16 streams. Several ports sharing
one budget can exhaust it: four fully populated ports require 8.192 Mbps.
The unchanged worker limit still rejects a 17th distinct live stream. A budget
that cannot fit advertised minima returns a typed error before allocating new
stream IDs; the existing port failure/autoreclaim path handles it. Low explicit
totals can therefore tear down a session. Targeted pause/refusal UI is deferred.

## Validation

Deterministic tests cover both-level caps, wide arithmetic, popup grouping,
first-frame allocation, frozen batches, generation changes, unchanged/focused
inventory, retirement, cross-port identity isolation, manual-actuator exclusion,
impossible minima, borrowed-drop recovery, boost/decay, atomic focus handoff,
cross-port expiry and complete input-batch allocation before admission. No
Smithay or codec internals are unit-tested by these cases.

The user reported a successful 120-second isolated AV1 network run on
2026-09-10, with multiple applications feeling responsive. The retained run
`network-hoist-z5p_dpb4` recorded roughly 25.31 MB of encoded video at both
endpoints and 30.76 MB of receiver-interface RX over about 122 seconds. Source
logs show activity-dependent group target redistribution; their sampled codec
failure counters sum to zero. This is acceptance evidence for that workload,
not a general hardware guarantee or a controlled quality comparison.

The 8 Mbps target would represent 120 MB over 120 seconds if continuously used.
These counters confirm lower actual traffic, not its complete cause: sparse
updates, compression demand, delivered cadence, unapplied targets and encoder
undershoot must be distinguished before attributing the gap to efficiency.
Per-group logs currently show requested/applied targets, not actual per-app
payload totals. Shutdown also produced client teardown errors; those are not
evidence of a fault-free application shutdown.

Broader hardware validation remains pending. Run short AV1 and H.264 sessions,
add/remove windows, open/close popups, resize and reclaim; stop at the first GPU fault. The
default-on change makes runtime encoder replacement a normal path and is not
evidence that the previously observed VCN fault is resolved. Review
`weld_media_diag` requested/applied sums separately rather than interpreting
either as measured wire throughput. Its source-side `encoded window bitrate`
records group identity, allocation priority, layer count, input pixels,
requested/applied targets and pending stream count on the existing reporting
cadence. No titles, key codes or input text are included. Compare Blender, BBB and
foot together: typing or sustained motion should transfer quality to the active
group, while focus alone gives a modest bonus. Also check brief pauses, rapid
focus changes and tiny popups without assuming every policy revision implies
an encoder replacement.
