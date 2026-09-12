# Shared native host runtime

Status: implementation in progress. The tracked regression fixture, common
runtime, optional policy/composition split and shared callback ledger are
implemented. The dedicated `SessionHost` loop is removed. Physical inactive
composition optimization, live Iroh admission and reconnect remain follow-ups.

See [current headless hosting](headless-host.md) for shipped behavior and
[receiver follow-ups](receiver-decoder-pool.md#portable-execution-boundary) for
the phone/Godot sequence this work prepares.

## Decision

Headless is an assembly choice: logical output information, application hosting,
and optional remote-session policy, without a local presenter. It is not a
different Wayland client lifecycle or a core-global mode.

The target is one core runtime used by nested, DRM, and presentation-free
assemblies. `--backend headless` remains convenient entrypoint terminology and
maps into ordinary runtime configuration. No replacement `HeadlessDriver`,
no-op renderer, or parallel session-only service loop is introduced.

Moving or hoisting a window does not itself detach its physical output. Output
attachment, window placement, and surface consumption are separate decisions.
Individual-surface hoisting consumes client buffers directly; it need not
compose or stream a whole logical output.

## Evidence and responsibility boundaries

Before this refactor, `backend/nested.rs`, `backend/drm/host.rs`, and `session_host.rs` each owned
an outer loop, `ClientRuntime`, dispatch, shutdown and child maintenance.
`runtime::service_client_adapters` shared ingress ordering, not runtime
ownership. `CompositionHost` combined policy, rendering, capture and native-use
completion. `RenderContext` required graphics resources even though hosting
clients does not always require them.

DRM already preserves logical output configuration through session pause and
activation. Its `ActivePhysical` / `InactiveOwned` choice is not general
connector hotplug or arbitrary presenter replacement.

| Owner | Responsibility |
| --- | --- |
| Common core runtime | Wayland display/socket and `ServerState`, client adapters, calloop dispatch, signal handling, ordered effects, child maintenance and readiness sources |
| Logical output state | Stable `OutputId`, configured geometry, scale and layout; independent of a native attachment's availability |
| Native drivers | Winit pumping or DRM/libinput/session events, native presentation deadlines, target leasing, presentation completion and platform cursor handling |
| Optional application integration | Policy event intake, raw-input filtering, paced policy updates and typed results; optionally composition/capture through the same integration owner |
| Distribution | Which drivers, policy and rendering capabilities exist; virtual output defaults, initial window policy, application launching and remote-session consent |
| Hoist core/bindings | Session admission, surface forwarding, authorization, codecs and transport; no desktop placeholder dependency for a source-only session |

No new crate is required by this plan. Native driver contracts remain internal
unless an actual external consumer needs one. Interface names below describe
responsibilities, not a mandate to add one public trait per row.

Pre-refactor design assessment: 6/8 checklist rows, approximately
7.5/10. The failed rows are a single describable module responsibility (each
backend mixes driver work with host lifecycle) and changing an implementation
without affecting callers (service changes must be coordinated across loops).
Common runtime ownership and the policy/composition split address those gaps;
the score was not a claim that the refactor was already implemented or verified.

The implemented runtime boundary now satisfies those two ownership checks:
8/8 rows for this slice. Backend code no longer owns independent client-service
loops, and policy/rendering borrow one integration owner. This design assessment
does not substitute for runtime tests or claim the deferred output and transport
work is finished.

## One loop, preserving actual backend ordering

The common loop has named phases, rather than an untyped collection of callbacks.
Backend-owned queues retain typed events inside core. Native callbacks never
re-enter Bevy or application policy.

| Phase | Nested | DRM | No local presentation |
| --- | --- | --- | --- |
| Prepare dispatch | Pump Winit; apply pending input, resize and scale; preserve early host-close exit | Mark pending policy-input work when its vblank-derived update deadline arrives | No native prework |
| Choose deadline | Preserve zero timeout after draining host input/geometry; otherwise current composition deadline | Minimum of existing application/frame deadline and active physical presentation schedule | Virtual callback deadline when needed, otherwise bounded child-maintenance wait |
| Dispatch and apply native events | One calloop dispatch; no second Winit pump | Ordered session transitions, vblank retirement and libinput conversion | One calloop dispatch and signal handling |
| Service clients and policy | Common ordered service, then installed/due policy and its effects | Same common service; native host effects remain at their existing effect position | Same service; application integration is optional |
| After service | Existing render/stage, cursor, present/capture and completion order | Existing cursor feedback, owned/physical route, due-output composition batch and retirement order | Virtual callback opportunities through the common completion authority; no renderer |

Common client service preserves completion-before-ingress, cursor feedback,
event observation, pending effects, server work and resize flushing. After a
policy advance, preserve client requests, pointer routes and adapter commands
with their intervening server-work drains, followed by host commands, cursor
updates and final resize flushing. Keep VT switching at its current point after
cursor policy updates. Flush/reap/shutdown retain their existing observable
positions, including nested's exit before dispatch when its host closes.

DRM pause ordering remains target availability change, input cancellation,
host-focus loss, libinput suspension and native pause. Activation retains
libinput resume, native activation/completion reconciliation and redraw demand.
Do not normalize these into a different ordering merely to shorten a trait.

Cursor operations must not disappear into an ambiguous “update cursor” hook:

| Operation | Required location |
| --- | --- |
| `ServerState::flush_cursor_feedback` | Shared completion-before-ingress service |
| Policy `take_cursor_update` | Policy-result application, retaining native backend ordering |
| DRM `set_cursor_position` | Input conversion and output-scale reconciliation where they occur today |
| `ServerState::take_cursor_image` and native presentation | Nested after policy/composition; DRM before its composition/presentation phase |

Acceptance is **no reordering of observable per-backend operations**. Input and
transport effects remain independent of the paced main/render work; no new
60 Hz input gate or network acknowledgement is introduced. Any unavoidable
ordering change requires its own reviewed amendment and latency evidence.

## Policy, composition and GPU ownership

Split the current `CompositionHost` responsibilities along its existing method
boundary. Policy owns event intake, input filtering, main advancement, debug
service, output-topology reconciliation and typed results. Optional composition
owns `render_outputs`, renderer-use completion, rendered-content readiness and
capture. Unsupported capture is an explicit error, not a dummy success.

One integration owner may implement both interfaces. `AppShell` retains one
Bevy `App`; the runtime borrows its interfaces sequentially. There must not be
two owning references to that app or new locking/interior mutability merely to
make the split compile. An adapter-only runtime needs neither interface.

Some native effects require simultaneous access to policy, server, clients and
driver. In particular, DRM output-scale changes currently update all four.
Store them as separately borrowable fields and construct a narrowly scoped
effect context from disjoint field references. Do not hide them behind a
whole-runtime mutable borrow and then recover access with `RefCell` or mutexes.

GPU import capability is independent of local presentation. Reuse
`request_weld_device`, the source cache and `DmabufContext` as common native
bootstrap, including the unavailable/SHM path. Bootstrap consumes a selected
adapter; it does not invent a global adapter-selection mode. Nested selection
must remain surface-compatible, DRM selection must match the opened device,
and presentation-free assembly may select an adapter without a surface.

Only a real compositor renderer requires `RenderContext`. A codec still needs
its own positively established capability. “No local presenter” does not mean
“no GPU,” and “SHM clients work” does not mean hardware encoding works. The
distribution binary may still link Bevy; binary feature splitting is not part
of this runtime refactor.

## Logical outputs and presentation

Represent a local attachment independently from logical output configuration:
an output can have no attachment, or a registered attachment whose availability
changes. A suspended DRM attachment is still registered; VT switching must not
destroy and recreate the native device.

Retaining an output preserves its identity, geometry and client session. Native
presentation resources retire through ordered driver teardown and the existing
completion machinery. Removing the logical output is a separate policy action.
Connector metadata and current presentation resources must not become the owner
of application lifetime.

The initial scope is zero or one local attachment per logical output, including
existing multi-output DRM presentation. It exercises current pause/activate and
unavailable-output behavior. Full connector hotplug, asynchronous replacement,
attachment generations and cross-GPU migration are deferred until an actual
event producer requires them; do not add placeholder machinery for those cases.

Finite virtual output information is compatibility metadata, not a bound on an
unbounded workspace. Initial logical sizes continue through client constraints
and are consumed once. Maximize/fullscreen should eventually use the active
receiver's presentation area, not an enormous advertised monitor. Codec
resolution remains independent of window size and application scale.

## Callback and rendering demand

Lift the existing DRM `CallbackLedger` into common runtime code. Retain its
pending-output sets, FIFO completed-prefix rule and output-removal handling.
It becomes the single callback staging/retirement authority. Native completion
and virtual timer opportunities feed that authority; a virtual timer must not
independently drain `ServerState`'s global callbacks.

This migration does not change `ServerState::stage_frame_callbacks` to a new
per-surface ownership model. The immediate validation targets are all-virtual,
nested, and DRM with its existing pause/activate behavior. Broader mixed-clock
assemblies require their own policy and validation before being exposed.

Keep physical retirement gating and buffer-release ownership intact. A callback
opportunity is neither a GPU release fence nor proof of remote receipt/display.
Currently unmapped surfaces remain ineligible as before.

Preserve inactive-owned/capture behavior while extracting ownership. Then stop
offscreen composition when no consumer requires it, with explicit no-render
tests and safe retirement of renderer-held leases. Do not force a dummy render
just to keep the host loop alive. Configured virtual callbacks may continue for
producer progress without local composition. There is no new continuous render
loop, and physical refresh is not imposed on protocol/input processing.

## Implementation sequence and acceptance gates

Steps 1 and 2 are implemented, along with the ledger extraction in step 3 and
CLI migration in step 4. The existing separation of logical `ServerOutput`
configuration from native output resources/availability remains intact.
DRM's inactive-owned rendering is deliberately preserved until renderer-held
leases can be retired safely without composition; this optimization is not
claimed by the runtime extraction. The presenter-free path never renders,
including when an application policy owner is installed. Steps 5 and 6 have
not started.

1. **Tracked regression fixture, before production refactoring.** Replace the
   temporary C-only evidence with a reproducible subprocess Wayland fixture and
   bounded runner. Cover initial min/max sizing, take-once/remap, callbacks,
   buffer releases, SIGTERM/socket cleanup and nested capture. Use private
   temporary runtime directories and child-process signal tests, not signalfd
   inside the multi-threaded Rust test runner. Keep the fixture in its own batch.
2. **Common runtime and optional policy/composition.** Migrate all three
   assemblies to one outer service loop using the phase ordering above. Replace
   `SessionHost`/`SessionHostConfig` with generic runtime/output configuration in
   the same coherent batch. Preserve adapter and wake registration, DMA-BUF
   context and external capability access. Do not stop at a type rename or
   another shared helper while independent outer loops remain.
3. **Common completion and optional presentation.** Lift the existing ledger;
   separate logical output attachment/availability; suppress unneeded local
   composition only with safe native-use retirement. Combine this with step 2
   if an intermediate cannot preserve coherent ownership. Native rendering
   algorithms and multi-output batching remain in their drivers.
4. **CLI assembly cleanup.** Keep headless defaults and validation at the
   entrypoint. Translate them into the common runtime without a headless flag
   in protocol, input, buffer or output lifecycle code. This is part of steps
   2–3, not another permanent backend.
5. **Requested Iroh demo.** Add an explicitly pollable/cancellable pending
   admission handle and wake integration in `weld-hoist-iroh`. Ticket publication
   and expected-peer checks retain their existing protections. Waiting for a
   peer must not block the running host or shutdown processing. Add the
   headless-to-nested launcher with foot/htop, Blender and an isolated Firefox
   profile. Install automatic source admission at construction, preserve one
   session per toplevel and relay-owned popups, and send the admitting commit
   exactly once. Require explicit whole-session consent and preserve shared
   bitrate budgeting plus receiver-owned repeat cadence/legacy fallback. This
   demo admits one initial authorized receiver, not multiple simultaneous viewers.
6. **Reconnect and retention.** Retain apps and last effective logical size/scale
   independently of a peer, release remote input, suspend unnecessary streaming
   and replay static windows to an authorized receiver. Preserve desktop reclaim
   semantics through explicit source policy. Smithay resets pending toplevel
   state on protocol unmap, so retained preferences cannot live only there.
   Reconnect and identity lifecycle need their own tests; step 5 does not claim
   automatic reconnect.

The demo defaults to direct Iroh on the same machine and has no hidden runtime
timer. Optional `--seconds` makes automation bounded. It publishes and verifies
the process groups it owns, restores inherited signal handling for the app
launcher, uses bounded TERM/KILL cleanup, disables core dumps, and preserves
logs. It must not reuse the normal Firefox profile or modify network routes.

## Affected code and verification

Primary refactor sites are `weld-core::{host,runtime,session_host}`, native
backend preparation/loops, `backend/drm/presentation`, output state and native
import bootstrap; `weld-app::{builder,shell}`; and the root argument/assembly
code. The demo additionally touches Iroh admission/host integration, hoist-core
constructor-time admission, scripts and run documentation. No vendored-source
change or blanket dependency update is planned.

Run narrow tests first, then:

```sh
cargo check -p weld-core -p weld-app -p weldwm -j2
cargo test -p weld-core --features test-support -j2
cargo test -p weldwm --lib -j2
cargo clippy -p weld-core -p weldwm --all-targets --features weld-core/test-support -j2 -- -D warnings
cargo fmt --check
git diff --check
```

Add focused application-contract and hoist/Iroh tests when those batches change
their crates. Test no application/no renderer, SHM-only operation, preserved
event/effect order, virtual retirement not completing physical work, logical
identity retained across availability changes, and zero composition calls
without consumers. Use the tracked subprocess fixture throughout the migration.

Physical checks include forced no-Vulkan startup, a native DMA-BUF client,
nested capture, then staged one-window and three-application Iroh runs. Check
relay failure as well as admission and receiver mapping; low-volume per-surface
diagnostics are preferable to per-frame trace spam. SIGINT during pairing and
receiver closure must clean up the test's entire process tree.

Real DRM pause/resume, multi-output pacing, cursor behavior and capture require
TTY validation. A passing fake-driver or nested test does not establish those
hardware results. No release builds are needed for this refactor.
