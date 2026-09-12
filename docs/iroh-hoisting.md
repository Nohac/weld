# Iroh hoisting

Weld can carry opaque encoded client surfaces between two compositor processes
over an authenticated and encrypted Iroh connection. The source and
destination use the same transport-neutral relay and encoded scheduling as the
Unix validation path; only connectivity and framing differ.

## Portable library boundary

`weld-hoist-iroh` and `weld-hoist-encoded` have compositor-free default dependency
graphs. `IrohNotifier` accepts the host's nonblocking, fallible wake callback;
the native feature adapts the existing Linux eventfd notifier. Input/control,
media framing, queue budgets and authentication use the same implementations.
This is not a new transport or a polling-based receiver.

`destination_registration_with_backend` accepts a decoder and matching
`DecodedFramePublisher`. The publisher supplies the app-side importer marker and
owns native lease construction; shared scheduling retains its typed decoded
buffers until atomic publication. The `native` feature adds Linux integration;
`vaapi` implies `native` and supplies the existing hardware-codec conveniences.
The standard distribution continues enabling VA-API, while an Android consumer
can depend on the default libraries without Smithay, Bevy, wgpu or libva.

Validation includes native-host tests with portable fake buffers and an Android
ARM64 compile check:

```sh
cargo ndk -t arm64-v8a --platform 24 check --locked \
  -p weld-hoist-encoded -p weld-hoist-iroh -j 2
cargo tree -p weld-hoist-encoded -p weld-hoist-iroh --target aarch64-linux-android
```

These are cross-compilation checks, not ARM64 test execution or hardware decode.
The existing private-file ticket/identity exchange remains available alongside
the persistent-device APIs below. QR/bootstrap UI and runtime reconnect policy
are separate work. Godot's native video fixture supports Android decoding and
GPU presentation, but its workspace is not yet wired to these receiver libraries.

## Persistent devices and saved connections

The shared library can retain the same authenticated identity across endpoint
restarts. `IrohDeviceIdentity::load_or_create` owns a private device directory and
its raw 32-byte `device.key`; `IrohHost::bind_with_identity` uses that key. Only
the public `IrohPeerIdentity` is exposed to callers or debug formatting. Existing
`IrohHost::bind` calls still generate an ephemeral identity; the validation
launchers have not switched to persistent enrollment.

The device directory must be current-user-owned with mode 0700, and the key
must be a regular current-user-owned mode-0600 file. Only the final directory
component is created; its parent must already exist. Symlink keys/directories,
special files, incorrect permissions and malformed key lengths fail closed.
Missing keys are atomically created without overwrite and synced to storage;
concurrent initializers use the winning key. Existing invalid keys are never
silently replaced. Secret read/write buffers are zeroized, not formatted as text.
This is same-OS-user trust, not secure-hardware storage or device attestation:
any well-formed 32 bytes is a valid key, and another process acting as that user
can replace it. Remote identity pinning then rejects the changed identity.
Back up or deliberately re-enroll a lost identity rather than bypassing pinning.

`IrohConnectionProfile` pins the source identity independently of address hints.
Its bounded text format is:

```text
peer=SOURCE_PUBLIC_ENDPOINT_ID
network=n0
address=192.0.2.10:4242
address=[2001:db8::10]:4242
```

Replace the example identity and addresses with the intended source's values.
`network=direct` requires at least one address; `network=n0` permits an ID-only
profile with discovery and relay fallback, and also honors optional address hints.
Current hosts bind ephemeral UDP ports: a Direct profile must be refreshed when
the source rebinds or its address changes. A stable identity alone does not make
those addresses stable; use N0 discovery for saved connections across restarts.
A bound host must use the profile's
network preset: loading a profile cannot silently turn on public discovery.
Persistent N0 identities are durable, linkable network-published identifiers.
Address hints never substitute for authentication or prove authorization.
Profiles reject unknown fields, repeated singleton fields, missing required
fields, more than 32 addresses and input exceeding 4096 bytes. `load` uses the
same verified private-file rules as rendezvous; `save_new` is atomic and refuses
to replace an existing profile. Deliberate profile updates/enrollment remain
the application's responsibility.

`begin_accept_trusted_source` accepts an explicit `IrohTrustedPeers` allowlist
(1–32 supplied identities), checking authenticated identity before sending any
Weld offer. `begin_connect_profile` returns a nonblocking, pollable
`PendingDestinationConnection`. Both admission and connection are deadline-bound
and cancellable; dropping an unclaimed successful result disconnects it too.
Only one incoming admission may be pending per host. The application still owns
active-viewer limits, retry/backoff, relay re-registration and cached-surface
replay. These APIs enable that policy without introducing a Godot-specific
transport or changing the wire protocol; they do not implement auto-reconnect
on their own.

## Same-machine validation

Run the same-machine tracer with:

```sh
scripts/run-iroh-hoist --codec av1 --network direct
```

Focus the source window and press `Super+H`. `h264` is also accepted. The
script starts two nested Weld instances, exchanges the intended endpoint
identities in a fresh private `target/validation/iroh-CODEC-NETWORK.XXXXXX`
directory, and writes separate `source.log` and `destination.log` files there.
It prints the paths and retains logs across runs; ticket and identity files are
removed on exit. `WELD_IROH_TICKET` is no longer supported. `direct` is
the default and disables address lookup and relays, so this validation has no
network-service dependency. `--network n0` enables N0 discovery, NAT traversal,
and relay fallback for later cross-network testing; startup fails clearly if
the endpoint does not become online within 30 seconds.

For the planned Wi-Fi/USB-tether test, see [Network validation](network-validation.md).
Its read-only preflight and intended-peer admission are available; the isolated
launcher and namespace address/DNS configuration are still prerequisites.

The current tracer has deliberate limits:

- One Iroh endpoint host owns the process identity, while each connection has
  independent peer state. The distribution still accepts or connects exactly
  one peer before its compositor runtime starts; dynamic adapter admission and
  reconnect are not implemented.
- Iroh authenticates the remote `EndpointId` and encrypts the connection. Weld
  admits only the intended destination identity supplied through a trusted
  local file; the destination authenticates the source identity from its
  trusted ticket. This is one-run transport identity approval, not the future
  Weld device proof, pairing UI, or mesh grants. These launchers still use
  ephemeral secret keys; the library's persistent-device API is opt-in.
- Only opaque encoded surfaces are supported. The source selects AV1 or H.264;
  the destination accepts it only when its hardware decoder and video
  processing path support that codec. Native file descriptors cannot cross the
  network binding.
- Control and input share one reliable bidirectional QUIC stream. Encoded
  access units use a separate source-to-destination stream so media flow
  control cannot block lifecycle or input records.
- Encoded commits have no application ACK. One global in-flight encode uses
  local media headroom, with ownership-preserving Busy admission and independent
  bounded control/media queues. The receiver reserves media capacity before
  allocating payloads and pauses admission when full. Write completion wakes
  static sources. QUIC reliability and congestion control remain unchanged;
  [these bounds](ack-free-streaming-plan.md) are not latency guarantees.

## Intended-peer admission

Manual source launches require `--hoist-iroh-expect-peer PATH`; the file contains
the destination's public EndpointId, not its private key. Destination launches
require `--hoist-iroh-publish-identity PATH`, published immediately after binding,
before GPU probing or waiting for the source. The source publishes its ticket
before waiting for that identity. `run-iroh-hoist` arranges both paths without
parsing keys or implementing a second authentication protocol.

Both publications must be regular, current-user-owned files with mode 0600 in
current-user-owned mode-0700 directories. Reads are bounded to 4096 bytes and
reject symlinks and special files; directory-relative operations hold the checked
directory open. Publications are atomic and never overwrite existing paths.
Use a fresh directory for each run. This trusts the local OS user to select the
peer: anyone acting as that user can supply a different identity. The files do
not prove a remote device's mesh membership, nor is ticket secrecy authorization.

The source checks Iroh's authenticated identity before opening or writing the
Weld bootstrap offer. Up to eight candidate attempts may run concurrently;
each gets five seconds for TLS and bootstrap. Rejected or timed-out attempts do
not consume the intended session, and losing established connections are closed.
Weld logs only the first three candidate failures plus a final count. These
bounds are not a guarantee of availability under an ongoing network DoS attack.

`--hoist-iroh-timeout SECONDS` bounds rendezvous plus admission/connect together
(default 120; range 1..3600). The script forwards its `--ticket-timeout` value to
both peers. Endpoint binding/N0 online waiting and GPU initialization precede
that budget; this is not a complete-process watchdog. No wire revision change
is needed: the existing exact revision, role, and codec checks remain in place.

## Connection diagnostics

### Queue pressure

Iroh control ingress admits at most 256 records per peer inbox. A full inbox parks
the async reader until the compositor drains a batch; it does not disconnect.
Each record owns its capacity permit, and terminal peer closure wakes parked
readers. Encoded media has a separate two-record admission queue; control does
not share those permits. Draining never waits for decode completion.
Opposite-direction stream work remains independently polled. Host notifications
remain level-triggered eventfd writes for every admitted record.

The destination's outgoing control queue retains at most 256 records plus the
record currently being written. Adjacent absolute pointer motions for the same
session and exact layer target retain only the latest position and timestamp.
Keys, buttons, scroll, gestures, pointer leave, acknowledgements and all other
control records are barriers; in-flight records are never rewritten. This is
the same adjacent-motion rule as `ApplicationInputBuffer`, applied to transport
backlog rather than introducing an input sampling timer.

Sustained non-coalescible outgoing overload is still terminal: synchronous
compositor callers cannot wait, and losing a release or growing an unbounded
queue is not acceptable. The error includes message-kind totals and coalesced
motion count. Source control/media retain their existing hard bounds. End-to-end
retry admission for these synchronous APIs is separate work; this is not a claim
that all network stalls or overloads are solved.

`weld_network_diag` reports parked incoming admissions, currently parked readers
and the longest **completed or cancelled** admission wait, at most once per
second when the host drains and pressure changed or readers remain parked.
That completed-wait maximum does not measure an ongoing wait.

`Iroh outgoing input summary` reports cumulative counters per destination
outbox, at most once per second on successful writes:

- Received records by kind: input, request, cursor acknowledgement, buffer
  release, reclaim; received, coalesced, and successfully written motions.
- Dequeued records versus completed writes, and completed framed bytes
  (including the four-byte length, excluding QUIC/IP overhead).
- Current queue depth and lifetime queue high-water, excluding the one
  in-flight record.
- Maximum retained-event wait before dequeue. Coalescing replaces the age as
  well as the position, so this measures the newest retained event, not the
  oldest superseded position.
- Maximum local serialization/write wall time for a completed record. QUIC
  accepting a write does not mean remote receipt, client dispatch, or display;
  this is not RTT. A still-blocked or failed write does not enter that maximum.

Subtract counters and `uptime_ms` between summaries to calculate interval
rates. Maxima are lifetime maxima, not per-second samples. A short burst
followed by idle may leave its final partial second unreported. There is no
idle reporting timer, input payload logging, or video trace.

Both control readers and writers reuse their framing scratch buffers, growing
only to the largest record seen; writers submit header and body together.
Decoded records own their contents before the read buffer is reused. Wire
format, input ordering, coalescing and notification behavior are unchanged.
There is still no motion-rate cap: a fast writer can forward device-rate input.
The next pacing decision should use these measured rates and queue delays;
neither display cadence nor a proposed 4 ms interval has been imposed here.

### Selected paths

```sh
RUST_LOG=info,weld_network_diag=debug scripts/run-iroh-hoist --codec av1
```

The optional observer uses Iroh path events for selected-path changes and reports
`ipv4`, `ipv6`, `relay`, or `other`; it resnapshots if events were missed. A
five-second timer reports selected-path RTT. It records public peer/path IDs,
not raw IP addresses, tickets, private keys, input, or video. A relay report
describes the relay transport, not the relay socket's IP family. The observer
holds only a weak connection handle, exits on close, and never wakes Bevy. With
the diagnostic target disabled, no observer task or timer is created.

Direct loopback tests cover rejection without exposing the offer, concurrent
silent attempts, retry after timeout, cleanup on overall timeout, and the
existing independent control/media exchange. Live N0/mobile validation remains
separate. These diagnostics do not yet log launched application exit statuses;
a Firefox window disappearing is still not by itself proof of transport loss.

`Iroh EndpointId`, `HoistEndpointId`, and `ClientSourceId` are intentionally
different identities. The first authenticates a network endpoint, the second
selects process-local hoist orchestration, and the third namespaces client
surfaces inside one Weld runtime.

## Opaque window and popup validation

Opaque encoded toplevels use destination SSD and display the client's declared
window geometry, excluding external shadow margins. Firefox's own tab strip
and controls remain inside that crop beneath the destination header. Popups
stay undecorated and independently streamed, with their own geometry crop;
they may extend outside the parent window. Transparency inside the declared
geometry, including translucent rounded corners, remains unsupported.

Run `scripts/run-iroh-hoist --codec av1`, open Firefox with `Super+F`, and hoist
it with `Super+H`. Check tab previews, the hamburger menu, context menus, and
submenus near both sides of the window. They should follow their anchors,
remain clickable outside the window, and close normally. Resize and move the
receiver, repeat the menus, then reclaim. Compare with `scripts/run-local-hoist`
and its `--native` mode, and check Foot and Blender for decoration regressions.
Both endpoints must be rebuilt together after the protocol revision change.

The current crop is applied at presentation; full buffers still pass through
the encoder. Removing that unused encoded margin is a later optimization and
must preserve geometry, scale, viewport, and input mappings across commits.

## Input and partial-surface stalls

Client cursor feedback (including custom images) travels separately from video.
See [Cursor feedback](cursor-feedback.md) for the protocol revision, local sizing
policy, and hover/resize/reclaim validation sequence.

The September 5 investigation found missing input cleanup on destination host
focus loss and source withdrawal, lost key releases across focus changes,
popup grabs that could block explicit focus clearing, and encoded coalescing
that erased unmap/remap boundaries. Regression tests cover Weld's routing and
relay lifecycle; Smithay grab behavior still needs live validation.

A separate explicit-sync defect discarded leftover release points when a
cached buffer assignment had been removed or had no new buffer to import.
Those points are now signalled without changing the lease-driven completion
of buffers actually sampled by the GPU. This can prevent a client buffer-pool
stall, but has not yet been confirmed as the cause of Firefox's frozen toolbar.

If the toolbar or resizing stalls while website content continues, reproduce
with `RUST_LOG=warn,weld_input_diag=debug,weld_surface_diag=trace scripts/run-iroh-hoist --codec av1`.
Click a tab, the address bar, and page content; resize; then reclaim and repeat.
The normal source/destination logs include button filtering, picking, source
grab state, and frame-callback progress. The opt-in trace logs no keyboard text
or video dumps. Stop the run after reproduction to keep the capture small.
Unmapped-surface callback throttling and cursor-animation callbacks remain
separate follow-up work; this fix does not enable unconditional callbacks.

## Popup teardown and disconnects

The September 5 popup investigation reproduced a teardown deadlock: the source
relay removed a destroyed popup's route, while the encoded scheduler queued its
destruction behind an acknowledgement that could no longer pass that route.
That original fix let destruction bypass credit; commit ACKs have since been
removed entirely. Destruction cancels unpublished pending frames. Codec work
already submitted retains its input lease until completion, then discards the
obsolete result and retires the active generation. Retirement must not race
ahead of a job that can still create that generation.

On the receiver, only media that has not arrived needs a late-frame marker.
Already-decoded layers are dropped directly; active decodes are cancelled and
retired after completion. Cancelled results cannot resurrect a popup or fail
the session merely because obsolete codec work returned an error. Completion
tokens and late-media session identities are still validated.

Regression tests exercise 160 consecutive surface teardowns, including
pending media and partial multi-layer decoding, without commit ACKs. For live
validation, repeatedly move between Firefox tabs to open and dismiss previews,
then exercise menus, resize, and reclaim. Old previews should disappear and
the hoist should remain connected.

The follow-up run with improved logs confirmed encoder stream-budget exhaustion
as the initiating failure, while popup previews were now disappearing correctly.
It exposed a second contract mismatch: Wayland snapshots omit vanished layers,
and normal rendering treats the buffer list as the complete inventory, but
encoding retired only explicit `Removed` entries. Both encoded endpoints now
reconcile that complete inventory. The source retires omitted layers before
submitting replacement layers, preserves `Retained` layers and other surfaces,
and clears an empty unmap inventory. A 160-commit rotating-layer regression uses
a strict fake encoder budget without destroying the surface. The exact mix of
Firefox unmapping and child-layer churn in the live run was not logged.

Relay failures log their initiating reason, and encoded/Iroh errors retain
nested context. Normal warning-level
logs are sufficient for another disconnect; optional
`weld_hoist_core::relay=debug` also identifies ignored stale messages by
surface, session, and message kind without logging input payloads.
