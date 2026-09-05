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
script starts two nested Weld instances, atomically publishes a temporary Iroh
endpoint ticket under `target/validation`, waits for the destination to
connect, and writes separate source and destination logs there. `direct` is
the default and disables address lookup and relays, so this validation has no
network-service dependency. `--network n0` enables N0 discovery, NAT traversal,
and relay fallback for later cross-network testing; startup fails clearly if
the endpoint does not become online within 30 seconds.

The current tracer has deliberate limits:

- One Iroh endpoint host owns the process identity, while each connection has
  independent peer state. The distribution still accepts or connects exactly
  one peer before its compositor runtime starts; dynamic adapter admission and
  reconnect are not implemented.
- Iroh authenticates the remote `EndpointId` and encrypts the connection. The
  initial listener accepts the first authenticated peer that reaches its ALPN;
  the explicitly shared ticket supplies reachability, not an enforced
  admission proof. This is not the future Weld device proof, pairing approval,
  or mesh authorization flow. The ephemeral secret key is not persisted.
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
