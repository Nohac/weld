# First cross-network hoist validation

## Status

The read-only inventory helper, intended-peer admission, opt-in path diagnostics,
and `scripts/run-network-hoist` launcher are implemented. The launcher's policy
and recovery sequencing have automated tests and its read-only preflight passed
on the selected host. A subsequent isolated AV1 run established a direct IPv4
connection and streamed until a GPU video-engine failure before the watchdog.
The launcher enforces interface separation and checks that the receiver has
only loopback plus the tether before starting peers. Together with the selected
path, this demonstrates operation across the isolated uplinks, not a completed
stability/recovery acceptance suite. The same-namespace launcher alone does not
prove that path.
The [implemented Iroh binding](iroh-hoisting.md) remains the baseline.

The first privileged attempt reached dhcpcd but aborted before launching either
Weld peer: the hook incorrectly classified an initial `NOCARRIER` as lease loss,
even though the journal immediately reported carrier acquisition. The launcher
reported successful tether restoration and host routing/DNS verification; its
protected recovery state was not independently read (sudo required a password).
This exercises an early-failure teardown, not the full internet-hoist lifecycle.
Initial carrier waiting is now nonfatal until a valid lease is configured. The
startup deadline remains the bound; terminal/validation errors and subsequent
lease loss stay fatal and cannot be hidden by a later hook event.

The first target is two nested Weld instances on the same laptop:

```text
source Weld + applications                    receiver Weld
host namespace                               temporary network namespace
wlp194s0 -> home Wi-Fi -> Internet <- phone mobile data <- enp197s0f0u1i1
```

Both host windows remain visible on the existing desktop. Only IP networking
is separated; the receiver keeps access to the same user's filesystem-backed
Wayland socket and GPU. Neither Weld process runs as root. This exercises real
WAN transport, but not different GPU hardware or a phone presenter.

## USB/ADB production validation

For the separate, opt-in **USB/ADB** path (not a WAN/tether test), see
[Iroh transport setup](iroh-hoisting.md#opt-in-adb-byte-stream-transport).
September 22 production validation used the normal source, receiver, AV1
decoder and XR presentation with only the Iroh packet carrier replaced:

- Desktop custom TCP loopback, Foot, 20 seconds:
  `target/validation/godot-hoist-b0lm84nj`; live decode/presentation, clean exit.
  This exercises the carrier but does not exercise USB.
- Pico USB, Foot, 60 seconds:
  `target/validation/godot-hoist-2t1iatro`; final status showed 50 decoded and
  presented frames, zero superseded. Foot was mostly static, not a throughput
  load. An earlier run that never gained OpenXR focus is not streaming evidence.
- Pico USB, Azahar stereo upper screen plus mono touchscreen, 90 seconds,
  24 Mbps shared encoder target:
  `target/validation/godot-hoist-fa3c_jcw`; selected path `adb`, roughly 10,000
  presented frames across the recorded layers, no reported QUIC lost packets
  or adapter queue drops in the sampled intervals. Receiver replacements still
  occurred, especially during startup; USB does not eliminate presentation or
  scheduling bottlenecks. The owned reverse mapping was absent after cleanup.
- N0 regression, the same Pico/Azahar setup for 30 seconds:
  `target/validation/godot-hoist-utqpty33`; selected direct IPv4, last status
  3,081 decoded and 3,002 presented frames, clean exit. This validates switching
  the persisted profile back to N0 without changing either device identity.
- Ctrl-C during connected Pico USB Foot playback:
  `target/validation/godot-hoist-xuiwnzgq`; launcher returned 130 after reporting
  completed cleanup, reverse mappings were empty and the shared ADB server PID
  was unchanged. Physical unplug recovery has not been exercised in this batch.

These are bounded functional runs, not long-session stability or input-to-photon
latency measurements. The Azahar run still logged Godot's `t->is_render_target`
diagnostic and unavailable GameMode service; both messages also occur in the
saved pre-change run `godot-hoist-eit7io07`. They were not transport failures.
The generated plots distinguish the ADB adapter's local queue drops from QUIC
loss and retain the existing source, decoder and presentation measurements.

## Read-only inventory

Run from an ordinary host terminal, without sudo:

```sh
scripts/check-hoist-network --tether enp197s0f0u1i1
```

Without `--tether`, it only lists candidates. USB ancestry proves a USB device,
not that it is a phone or using 5G. Confirm the selected interface and turn off
the phone's Wi-Fi. The helper does not mutate networking, resolve external
names, send probes, or read tickets, keys, SSIDs, MAC addresses, or USB serials.
Its output does contain IP addresses. Exit 0 means inventory completed, 1 means
the inventory environment is unusable, and 2 means invalid arguments or an
invalid/non-USB tether selection. None means ready to expose a listener.

On September 6, the host had:

- NetworkManager-managed Wi-Fi `wlp194s0`, IPv4 default metric 600.
- NetworkManager-managed USB tether `enp197s0f0u1i1`, IPv4 default metric 100
  and the only IPv6 default route. The tether could take over normal host
  traffic, not just Weld traffic.
- Phone DNS first in the resolvconf-generated `/etc/resolv.conf`; this was not
  a systemd-resolved loopback stub. Moving the tether without restoring host
  DNS could interrupt Codex and other host applications.
- No standalone `dhcpcd`, `dhclient`, or `udhcpc`; NetworkManager used its
  internal DHCP. No named network namespace or `/etc/netns` configuration was
  present. These are blockers to a naive namespace recipe, not reasons to copy
  the current lease blindly.
- Inactive Docker bridges. Preserve them and their firewall rules.

Recheck all of this before execution. Interface names, addresses, gateways,
DNS, connection settings, and device ownership are observations, not constants
to embed in a privileged helper.

The user was given an optional NetworkManager change to make the tether
non-default and ignore its automatic DNS on the host. This is not implemented
by the inventory helper. The first isolated launcher requires those safe
persistent settings so returning the tether cannot replace host defaults/DNS.
Before making that
change, record the exact profile's `ipv4.never-default`, `ipv6.never-default`,
`ipv4.ignore-auto-dns`, and `ipv6.ignore-auto-dns` values so they can be restored.
Do not assume the suggestion was applied; rerun inventory after applying it.
It disables automatic tether fallback rather than merely preferring Wi-Fi.
See [NetworkManager's settings reference](https://www.networkmanager.dev/docs/api/latest/nm-settings-nmcli.html).

## Batch 1: intended-peer admission and diagnostics

Transport admission and path diagnostics are implemented as described in
[Iroh hoisting](iroh-hoisting.md#intended-peer-admission). The original requirements
below explain the boundary. Application exit/disconnect diagnostics in item 6
remain follow-up work; they are not supplied by the network observer.

1. Each process generates its own ephemeral Iroh identity. The destination
   already authenticates the source ID from its trusted endpoint ticket. The
   source must also receive the expected destination ID through an explicit,
   trusted out-of-band exchange before accepting it. Publish identities before
   either side blocks waiting for the other; avoid a startup deadlock.
2. Check Iroh's authenticated `connection.remote_id()` before exposing the Weld
   codec offer, application metadata, input, or media. Never rely on EndpointId
   secrecy as authorization. Use Iroh authentication, not another homegrown
   key exchange. A shared local, private run directory can carry both IDs for
   this same-user test; a future cross-device test needs a trusted exchange.
3. Reject a wrong peer without consuming the sole intended session. The
   old `accept_source` accepted once and exited on bootstrap failure. Bound
   bootstrap time, rejection work/logging, and total startup waiting. Test a
   rejected peer followed by the expected peer and a silent peer timeout.
4. Do not log secret material. This one-run transport-ID approval is not the
   transport-independent device proof, grants, QR approval, or mesh trust from
   the [identity specification](spec/identity-and-meshes.md).
5. Record accepted peer identity, selected path changes (IP versus relay), RTT,
   and eventually bounded byte/queue summaries. The implemented observer reports
   path selection and RTT, not media throughput or queue accounting. Iroh 1.1.0 exposes
   `Connection::paths()`, `paths_stream()`, `rtt(path_id)`, and `stats()`;
   `Path::is_selected()`, `is_ip()`, and `is_relay()` distinguish selected paths
   from merely available ones. Use these, not a guessed path from the ticket.
6. Retain first-cause transport/codec errors. Add launched-client exit status
   and identifiable Wayland disconnect reasons so a disappeared Firefox window
   is distinguishable from lost transport. No keyboard contents, raw video
   dumps, per-motion logging, or unbounded trace capture.

Code evidence: `crates/weld-hoist-iroh/src/host.rs` and `peer.rs`,
`crates/weld-core/src/runtime.rs`, and `server/mod.rs`. Iroh is pinned to
`=1.1.0` in `Cargo.toml`; the path APIs above were checked in that installed
source. The [Iroh endpoint model](https://docs.iroh.computer/concepts/endpoints)
distinguishes cryptographic identity from changing reachability information.

## Batch 2: isolated launcher with bounded recovery

### Running the first validation

Run as the ordinary desktop user from the Rust development shell. First turn
OFF the phone's Wi-Fi. On the investigated host, dhcpcd 10.3.2 is already in the
Nix store but not on PATH; no installation is needed:

```sh
scripts/run-network-hoist --host wlp194s0 --client enp197s0f0u1i1 \
  --dhcpcd /nix/store/hwkp0y8nskmgbj02cnx97mny42vkn9bv-dhcpcd-10.3.2/bin/dhcpcd \
  --dry-run
```

Dry-run is read-only: no build, sudo, network probes, or system changes. It checks
current interfaces, routing, DNS, and the persistent NetworkManager profile.
Root-only checks (including PID 1 namespace identity where hidden from normal
users) repeat after elevation, before the move. The path above is a host
observation, not embedded in the launcher; use your installed dhcpcd elsewhere.

After reviewing the interface move and recovery behavior below, remove
`--dry-run` to authorize the actual test on the explicit `--host` and `--client`
interfaces. There is no interface-name confirmation prompt. The launcher builds
once without changing the Cargo profile or target directory, then asks for sudo.
Interface validation, recovery safeguards and the phone-Wi-Fi-off requirement
are unchanged. Both Weld processes and the source application run as you,
not as root. The runtime uses N0 discovery/relay services over the Internet.

The default is AV1 with foot, 120 seconds of startup budget and 120 seconds
after both Weld sockets are ready. Override with `--codec h264`, `--seconds 60`,
`--startup-seconds 180`, or a source application following `--`. Focus the source
window and press Super+H. Closing either peer, Ctrl-C, failure, or the deadline
stops the run and initiates recovery. Source and destination logs are printed
under a fresh private `target/validation/network-hoist-*` directory. Identity
exchange files are private and removed when the launcher returns; this is the
existing intended-peer approval mechanism, not a new authentication protocol.

Ctrl-C is owned by the privileged supervisor, which stops only this run's unit
and waits for its `ExecStopPost` recovery. The foreground launcher does not kill
the sudo child when interrupted; sudo can still show its initial password prompt.
Only the already-privileged `systemd-run` waiter is isolated from terminal signals.
Repeated interrupts cannot interrupt the stop/recovery sequence. Allow about
90 seconds, longer if the first recovery fails and its idempotent retry is needed.
A root-owned cancellation marker prevents late service registration from moving
the tether after an early cancellation. Failed terminal output cannot invalidate
successful cleanup or turn its final Python flush into exit status 120.

Recovery waits up to five seconds for NetworkManager to register and manage the
returning physical device. Unknown/unmanaged responses are retryable within that
bound; changed device identity or profile is not. Cleanup has a 70-second internal
budget beneath systemd's 90-second stop ceiling. No profile settings are changed.
These cancellation/readiness paths have deterministic tests; manual validation
must still check Ctrl-C both during startup and after both peers are ready, then
verify the original tether profile, routes and DNS after recovery.

### Isolation and recovery boundaries

The thin shell entry point delegates to `tools/network-hoist/`. Python's standard
library supplies process handling, validation, and state; no Rust/runtime
dependency or compositor code changes are involved. This is reviewed local
developer tooling executed with explicit sudo, not an installed privileged API
intended to defend against malicious edits by its invoking developer.

- The root supervisor stays in the original host mount and network namespaces.
  It releases only `--client` from NetworkManager and moves it to a unique named
  namespace. There is no veth, host NAT, bridge, or shared local IP shortcut.
- An independent transient DHCP service acquires a fresh **IPv4** lease with
  dhcpcd 10.x without PRIVSEP (validated inventory: 10.3.2). Its runtime/lease
  directories are private tmpfs mounts, and a replacement hook writes only this
  run's resolver/status. Host DHCP leases, hostname, and resolver are untouched.
  IPv6 DHCP/RA validation is deferred; do not claim a verified IPv6 test from
  this launcher. It does not replay the old phone lease or reinstate an old host
  IPv6 default route.
- The DHCP service and receiver each enter a private mount namespace before
  binding the private resolver and entering the network namespace. The receiver
  then drops privileges before loading the captured Cargo runtime environment.
  Native, filesystem-socket Wayland is required; abstract Unix sockets and X11
  do not cross this network-namespace boundary.
- The normal-user launcher snapshots its helper code in the private run
  directory before build/elevation. The root recovery copy comes from that same
  per-run snapshot, with its file hashes recorded. Repository edits cannot change
  either side of a running test.
- The systemd supervisor has a hard lifetime independent of the invoking
  terminal. Its cgroup is stopped before `ExecStopPost` recovery; the separate
  DHCP unit is stopped before returning the interface. Recovery uses a
  root-owned code/state snapshot under `/run/weld-network/RUN_ID`, not later
  edits to the repository or a user-writable cleanup manifest.
- Recovery rechecks the original physical device instance, namespace ownership,
  and saved interface-bound profile before returning the interface to the host.
  It reactivates that profile with a fresh lease, without changing its settings,
  host routes, DNS, forwarding, or firewall. Empty DHCP mountpoint directories
  created by the run are removed only when their recorded inode still matches.
- A replaced/unplugged device, changed/absent profile, or unknown namespace holder
  causes conservative refusal, not a guess. Interrupted empty-namespace creation
  is recovered only from this run's unique creation intent, before any device
  release; an unknown temporary-directory inode is left untouched with a note.
  State remains for inspection. In particular, a newly plugged device with the
  same interface name is **not** claimed automatically. Fix/inspect the reported
  condition and retry the printed `scripts/run-network-hoist --recover RUN_ID`.
  The same cleanup path is used automatically and manually, with a 60-second
  command budget and a short bounded lock wait. A still-failing ownership check
  needs manual administrator inspection. Recovery state is under `/run` and does
  not survive reboot.
- "Tether restored, but host routing/DNS verification FAILED" is a distinct,
  nonzero outcome. It means the original profile is active and the namespace has
  been cleaned up, but host connectivity needs investigation. The same recovery
  command retries verification without reactivating the tether. Minor preserved
  directory leftovers are recorded separately and do not prevent successful
  network recovery.

The first manual acceptance run must check timeout/Ctrl-C, early peer exit,
original profile restoration, unchanged host DNS/default routes, and no live
run-owned service/namespace. Policy tests mock system commands: they establish
our ordering/refusal decisions, not that this host's privileged lifecycle has
already been exercised.

### Original design requirements

Do not extend the current `run-iroh-hoist` script by simply adding `--network n0`:
it launches both processes in the same network namespace. Separate namespaces
have separate interfaces, routes, and firewall state; a physical interface can
belong to only one at a time.
[Linux network namespaces](https://man7.org/linux/man-pages/man7/network_namespaces.7.html).

The launcher must:

- Require explicit selection of the tether and host uplink via `--client` and
  `--host`; invoking the launcher authorizes the move. Reject an
  unexpected device, occupied namespace name, unsupported manager, or ambiguous
  ownership. Capture the tether profile/management state and host routes/DNS.
- Build once as the normal user before touching networking. Create a private
  per-run directory for tickets, logs, and recovery state. Do not overwrite
  previous reproduction logs or launch Cargo as root.
- Establish bounded address and DNS configuration using an explicitly selected
  installed lease client. The subsequent investigation found the dhcpcd store
  path above; it was absent only from PATH. Do not start a competing DHCP client while
  NetworkManager still owns the device. Validate IPv4 and IPv6 independently.
- Temporarily release only the confirmed tether from its manager and move it
  into a newly owned namespace with loopback. Leave Wi-Fi, Docker, host firewall,
  and host forwarding settings untouched. Do not add a veth, host NAT, or bridge
  that gives the peers a local shortcut. Moving the tether removes its host
  routes; the existing Wi-Fi default remains without manual metric edits. The
  observed host loses its sole IPv6 internet route during isolation.
- Verify host DNS has reconverged to Wi-Fi-reachable servers before proceeding.
  Give the receiver namespace its own reachable DNS, not a copied phone/host
  loopback assumption. The implementation binds a run-owned resolver into a
  private mount namespace, then uses `ip netns exec`; it creates no `/etc/netns`
  files.
- Enter the receiver namespace and drop privileges before executing Weld with
  its existing user runtime directory, display socket, and GPU access. Preserve
  the debug build's dynamic-library environment. Test those accesses explicitly.
- Use separate source/destination launches. The existing CLI ingredients are
  source `--backend nested --hoist-iroh-listen <ticket-path> --hoist-codec av1`,
  destination `--backend nested --hoist-iroh-connect <ticket-path>`, distinct
  `--wayland-socket` names, and `--hoist-iroh-network n0` on both. Add the
  implemented admission mechanism from batch 1: source
  `--hoist-iroh-expect-peer <identity-path>` and destination
  `--hoist-iroh-publish-identity <identity-path>`. Exchange paths must be fresh
  and private; publish before waiting as the current same-namespace script does.
  This list is deliberately not a runnable unsecured N0 recipe.
- Choose N0 for discovery, NAT traversal, and relay fallback in this mobile
  topology. Separate networks do not universally require relays: globally
  reachable direct paths can work. Record what actually carries application
  data; enabling N0 alone proves neither relay use nor a WAN path.
- Start a 120-second session watchdog after successful setup, with separate
  bounded startup/cleanup deadlines. Stop only processes owned by this run.
  Stop namespace processes before moving the tether back and deleting its
  namespace. Restore its prior management/profile state, then verify host
  networking. Handle startup failure, Ctrl-C, peer exit, and USB unplugging.
  Do not blindly reuse a newly plugged interface with the same name.
- Keep a narrowly scoped recovery command/state record in case the launcher
  dies. Deleting a namespace name while processes still hold it does not free
  the device. Do not rely on an EXIT trap surviving SIGKILL.

Namespace DNS and lifetime details:
[ip-netns](https://www.man7.org/linux/man-pages/man8/ip-netns.8.html).
The first privileged execution requires approval of the exact interface and
restoration plan. Implementation and read-only validation change none of that
system state.

## Batch 3: prove the route, then exercise hoisting

### Observed isolated AV1 failure

Run `8e5554535a2344529337be5d5c6552d3`, with logs in
`target/validation/network-hoist-4933rv7l`, connected at about 15:34:32 UTC on
September 6. Iroh selected direct IPv4, usually around 25–35 ms RTT, with
occasional higher samples. At 15:36:19 the kernel reported a `vcn_unified_0`
timeout attributed to `weldwm` and reset the ring. FFmpeg then reported AV1
output-buffer mapping/submission I/O failure, the source relay failed, and the
destination observed peer closure. This was about 108 seconds after startup,
before the 120-second session watchdog. DHCP was stopped during the resulting
teardown, not as the initial failure.

No relay-selected interval was observed in this particular run; that is not a
claim about other runs. User confirmation that phone Wi-Fi is off remains part
of interpreting the topology. The root cause of the GPU fault is unresolved:
the logs do not distinguish a driver defect from a buffer/synchronization or
workload-timing problem. Budgeting may reduce pressure but is not a proven fix.
The [streaming-budget plan](remote-budgeting-plan.md) keeps this investigation
separate.

### Remaining acceptance work

Before launching media, verify the source namespace has no tether and the
receiver has only loopback plus tether. Verify routing/DNS separately, phone
Wi-Fi is off, and no inherited proxy path points through the host. Record
selected Iroh path and interface byte deltas during a bounded transfer. If
route evidence is ambiguous, stop; two functioning windows do not prove 5G.
Any external reachability probe must be explicit, bounded, and identified in
the run instructions rather than hidden in the inventory helper.

For each run, record codec, window/layer count, image extents, selected path,
RTT, throughput, frame cadence/age, coalesced work, and first failure. Start
with one foot window: typing, selection/cursor shapes, resize, and reclaim.
Then Blender with one settings window. Firefox menus/tab previews come last;
its September 6 disappearance left a Dismiss tombstone but no conclusive
process-exit or transport-failure evidence. Do not treat that as a proven
network fault. Close the receiver deliberately and verify source recovery and
release of held inputs; automatic reconnect is not implemented.

Historical September 6 constraints: the then-current single outstanding commit
coupled cadence to round-trip time plus processing, with one in-flight encode
globally and a per-layer AV1 bitrate rather than a shared target. Those are not
the current pipeline's limits. Later work removed commit-ACK pacing and added
shared budgeting and decode pipelining; see [Iroh hoisting](iroh-hoisting.md) and
[shared diagnostics](hoist-diagnostics.md) for current behavior and measurements.
Do not promise a frame rate or widen queues to hide latency on the strength of
this historical test; use measured current behavior.

Success means verified distinct uplinks, intended-peer-only admission, usable
input and presentation, bounded memory/logs, and reliable reclaim/cleanup. A
working relay path is a valid first success. Direct mobile hole punching,
different hardware, adaptive streaming, and the phone UI remain later tests.
