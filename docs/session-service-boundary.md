# Session and service boundary

Status: implemented boundary, 2026-10-17.

## Ownership

The application assembles transports and service implementations. A small
`weld-session` crate owns registered listeners, service dispatch, bounded task
supervision and cancellable outgoing operations. Its dependencies contain no
surface, codec, pairing, filesystem service or compositor implementation.

Service registrations identify a protocol and its concurrency budget. The
registered handler owns that protocol's admission, negotiation and live state.
The runtime uses the same service budget across listeners. A transport exposes
the registered protocol identifiers through its connection mechanism and yields
connections with the selected identifier. Transport drivers retain their own
I/O executor and cancellation guards.

Existing hoist services live in `weld-hoist-core`: pairing, application catalogue,
thumbnail requests, diagnostics and source/destination bootstrap. Hoist service
assembly owns these drivers and supplies them to the generic runtime. New
services can live in independent crates and register without extending a central
service enum or modifying a transport dispatcher.

Admission and delivery are separate decisions. Pairing authenticates remote
identities; explicit peer approval and local socket access are other admission
paths. Direct-buffer delivery requires descriptor transfer and compatible
endpoint imports. Encoded delivery negotiates a codec. Generic supervision
requires neither pairing nor encoded delivery. Import compatibility remains
validated by the buffer importer.

## Implemented sequence

1. Extract generic registration/supervision from the hoist-specific server.
   Exercise distinct listeners, an independently registered service, overload,
   unsupported service dispatch, cancellation and shutdown.
2. Move outgoing pairing and paired-device setup/lifetimes into shared hoist
   services. Leave connection dialing, lane creation and packet pumps in Iroh.
3. Move explicit-peer bootstrap exchange and candidate/session ownership into
   shared services, retaining transport-specific descriptor validation and
   address/ADB setup in the bindings.
4. Make native versus encoded delivery requirements explicit in shared
   negotiation, and preserve codec-free local bootstrap coverage.
5. Validate both bindings and all receiver consumers. Audit transport modules
   for session protocol exchanges, service task ownership and duplicated policy.

## Distribution boundary

Cargo features are additive. Applications choose service and transport crates;
runtime registration determines availability. Service authorization is checked
separately from availability. Future optional services such as file access and
forwarding depend on the session interfaces; the runtime does not depend on
them. Forwarding, FUSE and file-transfer implementations remain future services.

`weld-hoist-core` exposes an additive `sessions` feature, enabled by default;
its relay-only build works with that feature disabled. `weld-session` builds
independently with no Weld, media or compositor dependencies. Additional
per-service distribution flags can follow their own concrete dependency splits.

## Assembly

```text
application
  ├─ SessionRuntime
  ├─ Registry<connection type>: service identifier → handler + budget
  └─ transport listener/connector
       └─ connection → shared supervisor → registered service handler
```

`Registry::register` accepts handlers from independent crates. A service can use
the generic packet traits or require additional typed binding capabilities.
Identity evidence is optional at this level; pairing handlers explicitly require
it. Registering a handler makes it available; that handler applies its shared
service admission policy before disclosing protected data.

Iroh maps registered identifiers to ALPN and returns the selected identifier
without interpreting its service semantics. `IrohHost::bind_registered` accepts
an application-owned registry, and `IrohHost::connector` prepares a pinned peer
dial target. Its default facade assembles the existing hoist services. Service
registrations currently select connections; existing hoist sessions retain their
independent control, media, thumbnail and diagnostic lanes.

Direct-buffer versus encoded mode is negotiated by the hoist bootstrap. A native
mode requires binding support; endpoint import validation still checks actual
buffers. The local Unix binding checks peer credentials and transfers descriptors.
The Iroh binding supports encoded delivery. Mode selection remains explicit.

Pairing, device setup and direct bootstrap have shared timeout/cancellation owners.
Completed-but-unclaimed results retain cleanup guards, and losing source
candidates close their connections. Transport handshakes, packet pumps and ADB
watchers retain the endpoint executor. Diagnostic request loops run under the
shared supervisor with a bounded per-connection budget and current consent.

## Review criteria

The previous design scored 7.5/10 on the software-design diagnostic: module
responsibility and implementation independence fail because transport startup
owns service negotiation/lifetimes. The required correction is shared handlers
behind registered services and transport connection capabilities. Completion
requires inspecting the migrated callers as well as testing the runtime; moving
spawn calls alone does not establish the boundary.

Preserve authorization before metadata disclosure, cancellation of partially
transferred operations, cleanup of unclaimed successful results, independent
service/media progress and the transport executor's lifetime.

Current boundary review: 9/10, with all eight diagnostic rows satisfied. The
runtime hides supervision and result ownership; handlers hide service policy;
bindings hide connection mechanics. The remaining confidence step is physical
playback across the deployed receivers after the automated integration checks.

## Validation

- Independent local service registration, shared cross-listener limits,
  unsupported identifiers, cancellation, unclaimed results and shutdown.
- Real Iroh custom-service request/reply with an application registry and no
  pairing/catalogue setup.
- Approved-peer checks before metadata, silent candidate cleanup, native mode
  with an empty codec list, rejection propagation and destination deadlines.
- Same-user Unix native bootstrap, plus existing descriptor/control transfer.
- Real Iroh pairing, catalogue, thumbnails, revocation, delayed control records,
  direct/ADB media and independent diagnostic collection.
- Relay-only feature build and Linux/Android/Godot receiver compile checks.

The initial run hit the existing rendezvous test's one-second publication
deadline twice. An isolated syscall trace completed it in 50 ms, and the complete
bounded-concurrency regression run passed. Its storage/read implementation is
unchanged; the failure's timing cause was not established.
