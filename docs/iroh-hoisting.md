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
