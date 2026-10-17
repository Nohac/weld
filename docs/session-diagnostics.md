# Session diagnostics

`weld-diagnostics` records bounded structured evidence and produces deterministic
human explanations. It is shared by desktop Weld, Weld Mobile and Godot/XR.
It depends on Serde and JSON; transport, codec and presentation integrations
supply owned scalar observations.

## Desktop commands

Build `weldwm` and `weld-control`, then start the updated desktop. Running
compositors keep their existing code until restarted; no restart is performed
automatically. Choose your actual Wayland session name:

```sh
cargo build -p weldwm -p weld-control -j8
weldctl --session weld-0 diagnostics list
weldctl --session weld-0 diag explain
weldctl --session weld-0 diagnostics explain SESSION_ID --verbose
weldctl --session weld-0 diagnostics export SESSION_ID --output report.json
weldctl diagnostics explain-file report.json --verbose
```

Exports create a new private file and refuse to overwrite an existing one.
Offline explanation requires no running compositor. A session ID identifies
one authenticated transport connection; several hoists can share that connection.
Omitting the ID from `explain` selects the newest recorded session, whether
active or ended, and prints its ID. `diagnostic` and `diag` alias `diagnostics`.
Headless Iroh sources expose the same commands through a diagnostics-only control
socket; pairing and application browsing remain desktop services.

`explain` and `export` request the matching report from the other participant
before producing their result. Collection uses an independent, bounded stream
on an existing authenticated Iroh connection. A newer connection to the same
identity can collect a retained report from an earlier session. Requests time
out after three seconds and leave video/input running. One collection is allowed
at a time per local host, and a peer serves at most one request per second.

The CLI reports whether collection succeeded, was denied, timed out, or found
the peer/report unavailable. Local evidence remains available, and previously
collected peer evidence is retained and identified as cached when refresh fails.
Both instances must run the updated build. An offline phone must reconnect before
its report can be fetched; the desktop does not wake or launch a remote app.

## Phone and peer evidence

Build/install the updated APK with `scripts/build-mobile`. In the application
browser, **Session diagnostics** shows the latest saved session, or the current
session when no completed report has been saved. Reports are paginated; Android
Back closes the report view. The phone's last completed report survives app
restart in its private `files/weld-device/diagnostics.json` storage.
The saved incident takes precedence over live evidence. A clean reconnect used
to collect its peer report preserves that incident; a later failure replaces it.

The phone automatically serves sanitized session reports to its approved host,
restricted to sessions that host participated in. The same portable responder
runs in desktop and Godot receivers. Explicitly trusted development hoist links
also permit session-scoped collection in either direction.

For the reverse direction, allowing a paired device to fetch the desktop's
reports still requires the existing diagnostics grant (disabled by default):

```sh
weldctl --session weld-0 devices list
weldctl --session weld-0 devices diagnostics DEVICE_ID --enabled true
```

On the phone, **Share & collect peer report** explicitly uploads that session's
sanitized local evidence and requests the host's matching report. If disconnected,
use **Reconnect** to collect afterward. Both endpoints must run this implementation
for collection. Ordinary pairing, browsing and hoisting are separate permissions.

The responder checks the authenticated device and target session's ownership and
access policy; a paired desktop grant is checked afresh on every request.
Responses must match the requested session and opposite endpoint role, schema
and bounds. Knowing another session's ID grants no access.
The phone also checks the saved report's host identity before uploading it.
After successful collection, `weldctl diagnostics explain/export` contains both
reports. Disabling the permission affects subsequent collection requests immediately:

```sh
weldctl --session weld-0 devices diagnostics DEVICE_ID --enabled false
```

The phone restores its last completed report into the shared archive before
reconnecting, so the host can collect that incident after an app restart.
Collection targets the other participant in the selected session. It does not
traverse other devices or expose unrelated sessions, device-wide logs or files.
Godot's report UI/export integration remains separate from the phone browser.

## Evidence and limits

- Each endpoint keeps at most 256 scalar events per recorder; its first failure
  is preserved independently of ring eviction. Ended recorders stop changing.
  At several stage samples per second, retained detailed history is roughly a
  minute, not the whole session. First-failure context can therefore be evicted.
  The shared report archive retains the most recent 16 session recorders in memory. Host
  restart or archive eviction can make old peer reports unavailable.
- A dedicated TLS-exporter label derives the same public correlation identifier
  at each endpoint. It is never used as a credential. Reports contain no device
  names, app titles, raw logs, IP/MAC addresses, pairing material, input or video.
- Timelines contain local monotonic offsets. Wall-clock start times are context,
  not synchronization. Combined reports keep the two timelines separate and
  identify remote evidence as peer-reported.
- Iroh records selected path kind, RTT estimate, congestion window, byte counters,
  packet losses and congestion events. Counter comparisons reset on path epochs.
  Receive samples measure work admitted to the encoded port, not socket receipt.
- Shared codec ports record encode, pending-send, receive and decode observations.
  Presentation progress means desktop handoff selection, mobile GPU publication,
  or Godot native import respectively; none claims physical scanout completion.
  Stage timings overlap and cannot be summed into input-to-photon latency.
  The encoder's pending age measures the active batch; it does not include the
  residence time of all queued source commits.
- Rules distinguish observed deadline failures, queue delays and local frame
  discards from possible transport/processing contributors. Idle streams with
  no pending work do not generate a stall finding. Missing evidence is explicit.

Session-control deadlines remain unchanged: five seconds for client reads and
writes, fifteen seconds for host reads. The original failing operation is now
recorded before teardown, and closing an admitted client session
uses a session-close reason rather than `bootstrap not admitted`. Recovery and
display-aligned pacing remain independent follow-up changes.

## Validation

Focused tests cover bounded history, preserved first failure, idle-source and
path-reset rules, invalid imports, and real direct-Iroh session exchange with
permission denial/grant and unrelated-peer rejection. The phone storage test
checks offline reload and host-scoped collection. These tests exercise Weld's
rules and authorization rather than JSON serialization round trips.
Real-Iroh tests additionally cover desktop-initiated retrieval, live permission
revocation, restored-report collection after reconnect, wrong-owner denial,
request rate limiting and timeout isolation from other streams.

The Pixel installation smoke test captured a real session-read timeout, saved
its ended report in private storage, and successfully explained that report
through `weldctl diagnostics explain-file`. The device was locked, so the report
screen still needs a visual check. This validates capture and persistence, not
a fix for the underlying timeout.

Desktop-initiated collection was also validated during a live Pixel AV1 stream
from an isolated headless host. `diag explain` fetched the receiver automatically;
the exported bundle contained matching session IDs and receiver receive, decode
and presentation evidence. Pairing controls remained unavailable on that
diagnostics-only host.

## Resolved scenario: periodic Wi-Fi scan stalls (2026-10-08)

Status: user-validated workaround in one streaming session. Retain this case
as evidence for a future diagnostics revision; further validation should establish
how broadly the result applies.

### Symptoms and environment

Steam streamed from desktop Weld to a Pixel 8 Pro through Weld Connect had
recurring longer lag spikes. Connecting the phone to a concurrent laptop hotspot
improved general smoothness, but the longer spikes remained. The laptop used
MediaTek MT7925 (`mt7925e`), kernel 7.2.8 and firmware `20260813113118`, with
NetworkManager and wpa_supplicant. The hotspot and router uplink shared one radio
on 5 GHz channel 36. Router signal was approximately -78 to -79 dBm; phone signal
was approximately -36 dBm.

### Evidence and intervention

- Disabling uplink Wi-Fi power saving left the spikes reproducible. The driver
  reported power saving off during the follow-up capture.
- Passive `iw event -t` recording caught a full-band background scan from
  22:12:33.440 to 22:12:40.946 local time: approximately 7.5 seconds. The user
  reported another video spike during this capture.
- Concurrent timestamped pings measured router RTT averages of 3.2 ms before,
  32.7 ms during and 4.3 ms after the scan. Direct hotspot-to-phone averages
  were 36.7, 36.8 and 41.3 ms respectively. The scan affected router latency;
  the phone probe showed a less distinct relationship.
- The source's Weld diagnostic report recorded an IPv4 path-selection event
  during that interval. Endpoint reports retain independent clock origins;
  precise cross-endpoint timing and the chosen destination address were not
  established by this capture.
- Locking the router profile to its current BSSID and reactivating it, while
  retaining disabled power saving, produced user-reported resolution of the
  recurring spikes. The hotspot and phone association remained active.

NetworkManager's [BSSID setting](https://networkmanager.pages.freedesktop.org/NetworkManager/NetworkManager/settings-802-11-wireless.html)
disables background roaming scans. Its
[scan policy implementation](https://github.com/NetworkManager/NetworkManager/blob/main/src/core/supplicant/nm-supplicant-config.c)
also documents why scanning disrupts AP clients. The observed improvement makes
background scanning a strong contributor in this case. Driver/firmware scheduling,
shared-radio operation and transport path changes remain possible mechanisms.
The user's similar periodic stalls at home, with strong signal, remain a separate
reproduction target.

### Lessons for a future diagnostics revision

Preserve scan start/end, interface/radio relationships, power-save state,
signal/retry counters, driver/firmware versions and transport path changes when
explicitly collecting an incident. Keep measured correlations, user-reported
outcomes and proposed causes distinguishable. Record interventions and their
results so a failed power-save experiment remains visible alongside the successful
BSSID-lock experiment. This case is reference evidence for future tool work;
the current change records documentation only.

Related upstream reports provide comparison material:
[MT7925 contention latency #1108](https://github.com/openwrt/mt76/issues/1108)
explicitly ruled out scanning in its reproduction, and
[firmware regression #1128](https://github.com/openwrt/mt76/issues/1128)
tested weak-signal behavior with the same firmware build. Neither establishes
the mechanism behind this session's scan-associated stalls.

### Follow-up: a disconnected interface still scans (2026-10-17)

Long streaming stalls recurred with the uplink profile already locked to its
BSSID and radio power saving off. Inspection found the leftover `graphite-ap`
interface on the same physical radio as `wlp194s0`. NetworkManager reported it
disconnected, but it remained managed and administratively up, in station mode.

Passive `iw event -t` recording attributed a full-band scan to `graphite-ap`
from 15:02:15.552 to 15:02:22.386 local time, approximately 6.8 seconds.
This is the duration of the scan operation; off-channel dwell within that
interval was not measured. There was no simultaneous transport-latency capture
for this occurrence, so its relationship to a particular streaming stall remains
a hypothesis supported by the earlier incident.

Setting only `graphite-ap` unmanaged left the main Wi-Fi connection active.
No scan events appeared during the following 25-second observation. Bringing
that interface administratively down required a separate privileged command
and was left for the user. Longer playback validation remains outstanding.

The important diagnostic distinction is between a connection profile, an
interface and the physical radio shared by multiple interfaces. A BSSID lock
on the active profile can leave another managed interface scanning that radio.

### Follow-up: TDLS failures and delivery freezes (2026-10-17)

After the auxiliary interface was unmanaged, the user reported that the earlier
spikes had subsided but multi-second freezes remained. The laptop's supplicant
repeatedly attempted TDLS negotiation with a peer whose MAC matched the Pixel's
current Wi-Fi MAC. Both devices were associated with the same access point.
Several attempts logged `key addition failed` and `Failed to set TPK to the
driver`; others retried the handshake and later reported a missing teardown
entry.

Around 15:13 local time, retries ran from 15:13:28 through 15:13:32. The phone's
received-byte counter was unchanged between the 15:13:30 and 15:13:35 samples;
at 15:13:41 its RTT estimate reached 1.27 seconds. Presentation reported one
frame over a 14.5-second reporting interval. Frames arriving afterward decoded
in tens of milliseconds, with no codec failures in those samples. By 15:13:46
Iroh had selected its relay path. Device wall clocks were checked to roughly
one-second precision. This supports a delivery stall coinciding with TDLS
failure; a controlled disable/retest is needed to establish causation.

TDLS negotiates a direct radio link between stations associated with an access
point. Ordinary local IP traffic can instead pass through the access point.
Iroh's direct-versus-relay selection operates above this Wi-Fi link choice.

The requested experiment is to disable TDLS temporarily on the laptop. The
supplicant's [runtime control implementation](https://android.googlesource.com/platform/external/wpa_supplicant_8/+/refs/heads/master/wpa_supplicant/ctrl_iface.c)
handles `SET tdls_disabled 1`; `0` re-enables it. The installed NixOS service
exposes D-Bus but starts without a CLI control socket. Its D-Bus
`TdlsExternalControl` property selects external setup control and is not an
equivalent disable switch. Preparing the CLI test therefore requires adding
`-O /run/wpa_supplicant` through a temporary service override and restarting
the supplicant, briefly interrupting Wi-Fi. Preserve the original service
arguments and sandbox, then use `wpa_cli -i wlp194s0 set tdls_disabled 1` with
appropriate privileges. Check the result, association and subsequent TDLS logs.
The toggle is runtime state; interface recreation or service restart requires
reapplying it. Remove only the test drop-in to restore the service setup.
At documentation time, the override is prepared and execution and playback
validation are pending user sudo authentication.

## Planned: detect scan- and link-associated streaming stalls

Priority: high. Background Wi-Fi scanning is a significant latency risk for
interactive streaming on Linux. These incidents justify treating it as a
first-class diagnostic scenario; severity depends on the driver, firmware,
scan scheduling and shared-radio use. Implementation is deferred.

The future diagnostic should:

- Passively collect scan start/completion/abort events with interface and
  physical-radio identity, including disconnected and auxiliary interfaces.
  Diagnostic collection should preserve the network's behavior by avoiding
  scan-triggering queries.
- Correlate scan intervals and recurring scan patterns with locally timestamped
  RTT, delivery stalls and transport path changes. Report measured overlap and
  suspected contributors separately; periodic stalls alone establish a pattern,
  while scan events supply evidence for the explanation.
- Inspect the active profile's BSSID lock, effective power-save state and other
  managed interfaces sharing its radio. Record driver/firmware context and the
  observation interval so users can compare before and after a change.
- Capture TDLS setup, key-installation failures, retries and teardown events,
  correlating them with delivery interruptions and direct/relay path changes.
  Track a TDLS-disable experiment separately from scan suppression so their
  effects can be evaluated independently.
- Explain which endpoint and interface supplied the evidence through the shared
  diagnostics report and `weldctl diag explain`, with client UI presentation
  later. Keep platform collection separate from transport implementations and
  preserve session-scoped privacy when sharing evidence.
- Offer explicit, reversible instructions for suppressing background scans:
  lock the relevant NetworkManager profile to its current BSSID, and identify
  confirmed-unused sibling interfaces that the user can unmanage or bring down.
  Include previous settings, undo steps and verification. Explain the loss of
  roaming and any reconnect interruption before recommending a BSSID lock;
  establish interface usage before suggesting its deactivation. Changes remain
  user-approved actions.

NetworkManager documents BSSID locking as disabling roaming and background
scanning, with driver-dependent support in its
[wireless settings reference](https://www.networkmanager.dev/docs/api/latest/settings-802-11-wireless.html).
The guidance must account for scans initiated elsewhere on the same radio,
as demonstrated by the follow-up above.
