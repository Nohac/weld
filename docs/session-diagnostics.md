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
weldctl --session weld-0 diagnostics explain SESSION_ID --verbose
weldctl --session weld-0 diagnostics export SESSION_ID --output report.json
weldctl diagnostics explain-file report.json --verbose
```

Exports create a new private file and refuse to overwrite an existing one.
Offline explanation requires no running compositor. A session ID identifies
one authenticated transport connection; several hoists can share that connection.

## Phone and peer evidence

Build/install the updated APK with `scripts/build-mobile`. In the application
browser, **Session diagnostics** shows the latest saved session, or the current
session when no completed report has been saved. Reports are paginated; Android
Back closes the report view. The phone's last completed report survives app
restart in its private `files/weld-device/diagnostics.json` storage.
The saved incident takes precedence over live evidence. A clean reconnect used
to collect its peer report preserves that incident; a later failure replaces it.

Remote collection requires a separate permission, disabled for existing and
new pairings by default:

```sh
weldctl --session weld-0 devices list
weldctl --session weld-0 devices diagnostics DEVICE_ID --enabled true
```

On the phone, **Share & collect peer report** explicitly uploads that session's
sanitized local evidence and requests the host's matching report. If disconnected,
use **Reconnect** to collect afterward. Both endpoints must run this implementation
for collection. Ordinary pairing, browsing and hoisting are separate permissions.

The host checks the authenticated device, the current diagnostics grant, session
ownership, schema, role and bounds. Knowing another session's ID grants no access.
The phone also checks the saved report's host identity before uploading it.
After successful collection, `weldctl diagnostics explain/export` contains both
reports. Disabling the permission affects subsequent collection requests immediately:

```sh
weldctl --session weld-0 devices diagnostics DEVICE_ID --enabled false
```

The first collection flow is phone-initiated. Desktop-initiated retrieval from
arbitrary peers, mesh traversal and applet UI are future work. Godot contributes
transport, decode and presentation evidence through the same APIs; its report
UI/export integration remains separate from the phone browser.

## Evidence and limits

- Each endpoint keeps at most 256 scalar events per recorder; its first failure
  is preserved independently of ring eviction. Ended recorders stop changing.
  At several stage samples per second, retained detailed history is roughly a
  minute, not the whole session. First-failure context can therefore be evicted.
  An Iroh host retains the most recent 16 session recorders in memory. Host
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
- OS network-interface transitions, Wi-Fi scans, thermal state, frame-level
  cross-peer correlation and clock alignment are not collected in this slice.
  A report cannot confidently blame a router or radio from RTT alone.

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

The Pixel installation smoke test captured a real session-read timeout, saved
its ended report in private storage, and successfully explained that report
through `weldctl diagnostics explain-file`. The device was locked, so the report
screen still needs a visual check. This validates capture and persistence, not
a fix for the underlying timeout.
