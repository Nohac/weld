# Iroh hoisting

Weld can carry opaque encoded client surfaces between two compositor processes
over an authenticated and encrypted Iroh connection. The source and
destination use the same transport-neutral relay and encoded scheduling as the
Unix validation path; only connectivity and framing differ.

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
  Weld device proof, pairing UI, or mesh grants. Secret keys are ephemeral.
- Only opaque encoded surfaces are supported. The source selects AV1 or H.264;
  the destination accepts it only when its hardware decoder and video
  processing path support that codec. Native file descriptors cannot cross the
  network binding.
- Control and input share one reliable bidirectional QUIC stream. Encoded
  access units use a separate source-to-destination stream so media flow
  control cannot block lifecycle or input records.
- The encoded tracer permits one outstanding destination commit per surface
  and one global in-flight encode. This bounds memory and stale frames, but its
  throughput is currently coupled to round-trip time. Later media budgeting
  and feedback may widen or replace that credit policy based on measurements.

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

Iroh ingress admits at most 256 records per peer inbox. A full inbox now parks
the async reader until the compositor drains a batch; it does not disconnect.
Each record owns its capacity permit, and terminal peer closure wakes parked
readers. Destination control and media share this inbox, so control admission
can wait behind media pressure, but draining never waits for decode completion.
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
That completed-wait maximum does not measure an ongoing wait. No additional
timer, input payload logging or video trace is enabled.

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
Destruction now cancels pending frames and bypasses frame credit. Codec work
already submitted retains its input lease until completion, then discards the
obsolete result and retires the active generation. Retirement must not race
ahead of a job that can still create that generation.

On the receiver, only media that has not arrived needs a late-frame marker.
Already-decoded layers are dropped directly; active decodes are cancelled and
retired after completion. Cancelled results cannot resurrect a popup or fail
the session merely because obsolete codec work returned an error. Completion
tokens and late-media session identities are still validated.

Regression tests exercise 160 consecutive surface teardowns, including
withheld frame acknowledgements and partial multi-layer decoding. For live
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
