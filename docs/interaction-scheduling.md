# Interaction-aware encoded scheduling

## Scope

The encoded Unix and Iroh ports use the same activity policy and weighted
admission scheduler in `weld-hoist-encoded`. This chooses among queued work
within one port. It is not a bitrate allocator, FPS limiter, device-wide
multi-peer budget, or guarantee of input-to-presentation latency.

No codec settings, worker limits, wire records, or protocol revision change.
Already accepted GPU jobs and bytes in the ordered media stream cannot be
preempted. Decoder stream affinity and per-worker FIFO can limit or eliminate
the benefit for a particular workload. Busy candidates yield to other ready
work; the port does not guess at or reserve the backend's capacity.

## Activity and grouping

Only authorized relay input and focus requests supply attention. Application
commits do not promote themselves. Each endpoint measures recency on its own
monotonic clock; remote event timestamps do not determine priority.

The default `SchedulingPolicy`, replaceable when constructing either encoded
port, uses these initial tuning values:

| Class | Weight | Signal |
| --- | --- | --- |
| Background | 1 | No recent interaction or current focus |
| Focused | 2 | Focus alone, including repeated focus requests |
| Moving | 4 | Changed pointer position over the focused presentation |
| Interactive | 8 | Key press, accepted focused repeat, pointer press, scroll, gesture, active drag, or resize start |

Changed motion reaches interactive priority after 75 ms of continuous movement,
with gaps shorter than 150 ms. This uses time, not mouse event count. Initial,
unchanged, and layer-handoff positions do not promote attention. Plain motion
over an unfocused window does not promote it; clicks, scrolling, and dragging
still credit their actual target. Discrete interaction decays after 750 ms;
motion decays after 150 ms without changes. Releases, cancellation, cursor
feedback, and focus clears do not renew the boost. A held button alone cannot
keep a window interactive without movement. Attention tracks at most 32 held
buttons, independently of the authoritative input-release ledger.

Each toplevel and its popups form one scheduling group within its hoist session.
Layers and additional popups do not multiply that group's weighted entitlement.
Independent dialogs remain separate. Missing, cyclic, or cross-session popup
ownership falls back to independent grouping rather than following an unbounded
chain. Unmap/withdraw/destruction clear affected attention; a later mapping does
not inherit a stale drag. Removing a popup does not remove its owner's focus.

The source relay validates input before notifying the port. An unmapped focus
target installs no focus; a request in the correct source namespace carrying
any currently mapped session on that peer can clear port attention. Unknown
sessions cannot do so. This is a local observation, not a synthesized Wayland
focus request. A repeated key can renew activity only for the port's current
focused group, even if a prior press remains captured for eventual release.

## Admission and ordering

Weighted virtual service chooses the next group; stable queue rotation chooses
a member within it. Only successful admission spends service. A continuously
observed ready group waiting 100 ms wins oldest-first over weighted choice.
That is an aging policy at admission opportunities, not a 100 ms wall-time
deadline. Busy and missing-media work cannot block another usable candidate.
With expensive encode batches, the 100 ms aging override may give background
work more frequent service than the nominal 8:1 weight ratio. Compare
`aging_selections` with the per-class counts in manual runs rather than assuming
those weights describe measured throughput. Priority decay preserves service
debt; a genuine promotion may move a group back to the least-served position.
Group identities and attention are computed once into reusable scratch maps per
selection scan. Keyboard-focus changes reset motion continuity only for the
old/new groups and do not clear pointer-button holds on other windows.

The source remains work-conserving: an idle encoder starts the first available
commit immediately. Priority orders an actual backlog and never delays the
encoder to collect competing arrivals. One active multi-layer batch remains
nonpreemptible; unpublished commits still coalesce with their existing atomic
replacement and lifecycle rules.

Source polling drains completions and receives destination records with new
batch admission deferred. After the relay validates the entire received batch,
its required `progress_after_destination` hook enables selection again. Cursor
acknowledgement side effects may flush output during that batch but cannot
start a new encode. The phase also runs for an empty batch, so completion and
transport-headroom recovery progress without another key or client commit.
Disconnected relays do not resume work. Native and loopback ports explicitly
implement the hook without adding media scheduling.

On the receiver, metadata and already-decoded commits continue to publish in
their original per-packet lifecycle order. Only new decode admission waits until
the end of the currently available bounded transport drain. Publication is not
weighted. Front-commit order, atomic layer application, compatible-successor
lookahead, cancellation, and decoder-generation retirement remain intact.

Source and destination use existing wakes and polls. Lazy activity decay creates
no periodic idle wake, Bevy frame, extra timer, or per-input network packet.
Local shell movement is already drawn locally and does not require a media
boost. Configure resize-start observations are consumed, but the existing
source resize-encoding suspension is unchanged.

## Diagnostics and validation

`weld_media_diag=debug` emits bounded scheduling summaries on ordinary polls,
at most once per second after admissions. Each contains accepted selections
by class, aging selections, and group-count/oldest-candidate-age snapshots from
the last selection scan. These are not live GPU occupancy, socket receipt,
presentation timing, or an independent latency trace. Existing media queue and
worker timing summaries remain available. No key codes or text are logged.

GPU-free tests cover policy decay, motion frequency independence, group and
session boundaries, lifecycle clearing, weighted service and aging, Busy
ownership, relay rejection before observation, complete input-batch admission,
empty-batch progress, receiver prioritization and same-drain withdrawal.
Existing codec lifetime, generation, coalescing, and decode-ahead tests remain.

The user reported successful manual operation and subjectively snappier keyboard
input, including with a single hoisted window. This is acceptance feedback, not
a measured latency reduction or proof that weighted scheduling caused it.
For subsequent comparisons, run the normal AV1 local and isolated network
hoist tests with Blender and BBB. Compare input responsiveness, background
progress, and queue age under the same workload. Check focus changes without
input, sustained movement/typing, popups, resize, reclaim, and disconnect.
Do not interpret a successful session as proof of a speedup.

Shared bitrate/FPS allocation, user network preferences, physical-device
admission, more detailed latency tracing, and changes to decoder-worker
placement remain subsequent slices.
