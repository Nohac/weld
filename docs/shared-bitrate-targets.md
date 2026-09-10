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

Equal-weight presentation groups share the target. Each toplevel's layers and
owned popups form one group; independently managed dialogs form another. Groups
are scoped by source-port membership and hoist session. Layers divide their group
by full input-buffer pixel area, not logical/cropped window geometry. Codec
padding is not exposed here and is not included in that estimate.

Both allocation levels reserve backend minima and redistribute capped shares.
The allocator leaves 5% spare headroom where minima permit, rounds ideal rates
down to 64,000 bit/s steps, then avoids small optional changes. With a single
layer, the initial desired rates are therefore **7.552 Mbps AV1** and
**15.168 Mbps H.264**.

Existing targets are reduced only when needed to fit the configured total.
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

The existing interaction scheduler still prioritizes queued work. Focus and
input do **not** change bitrate allocations in this slice. Activity-weighted
quality allocation is the next separate policy change, with encoder-switch
churn to consider explicitly.

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
impossible minima and borrowed-drop recovery. No Smithay or codec internals are
unit-tested by these cases.

Hardware validation is pending. Run short AV1 and H.264 sessions, add/remove
windows, open/close popups, resize and reclaim; stop at the first GPU fault. The
default-on change makes runtime encoder replacement a normal path and is not
evidence that the previously observed VCN fault is resolved. Review
`weld_media_diag` requested/applied sums separately rather than interpreting
either as measured wire throughput.
