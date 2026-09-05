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
