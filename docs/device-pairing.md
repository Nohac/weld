# Phone pairing and running applications

Weld Master exposes a same-user local control socket for approving devices.
Weld Mobile can pair, browse running native-hosted application windows, hoist one
application family, and return it to the desktop. The source application's
process keeps running on the desktop throughout.

## Manual validation

Use the Rust/Android development shell. The phone needs network access to the
laptop; USB is needed only for installing this development APK.

1. Build the desktop and install the phone app:

   ```sh
   cargo build -p weldwm -p weld-control -j 8
   scripts/build-mobile --install --serial 38171FDJG009S5
   ```

   Substitute the serial shown by `adb devices` for another phone. Installation
   preserves the app's private identity and previous pairing. Opening Weld Mobile
   from its launcher uses paired mode; `run-mobile-hoist` explicitly selects a
   separate development profile.

2. For a first test, start an isolated nested desktop with a terminal:

   ```sh
   cargo run -- --backend nested --wayland-socket weld-phone-test \
     --config examples/mobile-pairing.sway.config -- foot -e htop
   ```

   Leave it running. This fixture has logical Alt+Enter to open another terminal
   and Alt+Shift+Escape to exit. For your actual desktop, restart Weld using the
   new build and your usual config instead; running applications in an older
   compositor cannot acquire this feature without restarting that compositor.

3. In another laptop terminal, display a short-lived invitation:

   ```sh
   target/debug/weldctl --session weld-phone-test pair
   ```

   Use `--session weld-0` for a normally named Weld desktop. The explicit session
   avoids accidentally using the outer Sway session's `WAYLAND_DISPLAY`.

4. In Weld Mobile, tap **Scan pairing QR** and point at the terminal QR. Google
   Play services supplies the scanner UI; its first use can require a module
   download. If unavailable, scan with another QR reader and open the `weld://`
   link, or copy the complete link onto the phone and tap **Paste pairing link**.

5. Confirm **Continue** on the phone. The desktop reports the requesting model
   name and public identity. Read the verification code from the phone and type
   it into the desktop terminal. An empty or wrong answer cancels approval.
   Invitations expire after two minutes; run `pair` again if needed.

6. Tap **foot** in the phone's application list. Verify that htop streams, taps
   select the expected row, and rotation resizes the application. The desktop
   should retain a hoist placeholder. Use Android's **Back gesture/button** and verify the
   original window returns; select it again to test a second hoist.

7. Open a second independent terminal in nested Weld. Both windows should appear
   in the list. Return the current application before choosing the other one.
   Close/reopen Weld Mobile and verify it reconnects without another QR. Also
   restart the test desktop with the same socket name and use **Reconnect** on
   the phone: its existing approval should survive.

8. List and revoke the test approval, preferably while an application is hoisted:

   ```sh
   target/debug/weldctl --session weld-phone-test devices list
   target/debug/weldctl --session weld-phone-test devices revoke FULL_PUBLIC_ID
   ```

   Copy the full public ID from `devices list`. The phone must disconnect, the
   source must reclaim its window, and **Reconnect** must fail until a new pairing
   is approved. Revocation survives a desktop restart.

The ordinary desktop and pairing commands have no test timeout. The separate
`run-mobile-hoist` development fixture remains bounded.

After connection setup, slow application-control reads and writes retain their
in-flight record until it completes or the connection closes. A warning marks
the delay and a recovery log records completion. Hoist/release actions likewise
wait for the desktop's answer while the connection is alive; catalogue polling
resumes after that answer. Dropping the phone
session cancels pending waits and closes its peer. Queued requests remain bounded
and ordered, while media and input use their independent streams. Initial
connection/authentication deadlines and malformed-record rejection still apply.
Authenticated sessions remain admitted while QUIC is alive, including during
control stalls; the endpoint retains its eight-session capacity limit.

Streams use the full display, including the camera-cutout area; system bars can
be revealed with Android's usual gestures. Back from the application list leaves
Weld Mobile. The phone requests tiled sizing, so host-side floating size hints
do not clamp its requested dimensions. Applications can still choose their own
content layout; a desktop application is not automatically a mobile-responsive UI.

## Trust and networking

- QR data contains the host's stable public identity, dialing hints, and a random
  one-use 256-bit invitation secret. Treat the QR/link as sensitive while live.
  The phone pins that host identity; Iroh authenticates the phone's public key.
- Model names are untrusted labels. Approval requires the code displayed on the
  intended phone, not merely a familiar name. Phone-side confirmation precedes
  enrollment; successful enrollment replaces the phone's one saved desktop.
- Approval currently grants browsing, thumbnail previews and interactive hoisting of running apps.
  This is powerful desktop access: a terminal or other controlled application
  can itself run commands. There is no dedicated remote-launch API or app whitelist
  in this slice, and this permission is not an application sandbox.
- Keys and trust are stored under `$XDG_DATA_HOME/weld/devices/SESSION` (usual
  fallback `~/.local/share`). The local socket is under
  `$XDG_RUNTIME_DIR/weld-control`. The phone uses its private app storage.
- First pairing enables Iroh N0 discovery/relay networking. A saved trust file
  restores listening on later desktop starts; revoking all devices removes their
  access but currently still restores the endpoint. Direct routes are used when
  available. Public DNS is used by this Android development receiver.
- [Google Code Scanner](https://developers.google.com/ml-kit/vision/barcode-scanning/code-scanner)
  requires Play services and uses Google's SDK and its
  diagnostics/telemetry behavior. Weld requests no camera permission. The
  dependency and packaging tool versions are pinned; Android Studio is unnecessary.

## Thumbnail protocol

`DeviceSession::services.thumbnail` requests one catalogue occupant using its window ID
and `ClientSurfaceId`. The optional cached revision produces an `Unchanged`
response when the host still has the same snapshot. Replies include snapshot age
and a stale flag, or `Pending`, `Unavailable`, or `Denied`. Clients scope cached
revisions to the current connection and discard previews when occupants disappear.

The transport-neutral records live in `weld-hoist-protocol::thumbnail`, with
service envelopes in `weld-hoist-protocol::service`. Iroh establishes an
independent bidirectional `weldsvc1` packet lane during paired-session setup.
Core's shared service driver exchanges a bounded Postcard header and a separate
length-delimited PNG packet through the binding's packet I/O interface.
Dimensions are capped at 320 pixels per edge and payloads at 512 KiB. Receivers
validate these bounds before allocation; image consumers must also bound PNG
decoding and verify the actual dimensions against the advertised extent.

`weld-hoist-core::session::SessionService` owns a shared, memory-only 32-entry cache.
Its bounded mailbox serializes changes on one worker. Authorized requests
coalesce into capture tickets; one ticket is current at a time, with a two-second
minimum refresh interval per target. The producer services
`take_thumbnail_capture`, marks composition changes with `invalidate_thumbnail`,
and returns an image or failure through `complete_thumbnail_capture`. Tickets
expire after five seconds. Removed occupants, ended access sessions and expired
tickets cannot publish late results. Unchanged catalogues skip cache pruning.
The producer must bound its GPU work and retain dormant-workspace behavior.

Each client queues at most one request behind its active exchange. The shared
service driver limits replies to four per second per lane. A ten-second transfer timeout closes only
the preview lane; catalogue, input and media continue. Reconnecting establishes
a fresh lane. Disconnect drops its pending requests. The cache retains at most
16 MiB of encoded snapshots, with shared references for transfers in progress.

Thumbnail access has its own `DevicePermissions::thumbnails` grant. Existing
trust records default it to false; pair again through the updated consent prompt
to grant it. Listing titles alone grants no pixels. Every request checks current
paired-device permission and membership in that connection's authorized catalogue.

This slice supplies and tests the protocol, cache and capture-producer boundary.
Native GPU thumbnail capture and Weld Connect preview cards are the next slice;
the desktop currently has no thumbnail producer. Rebuild paired client and host
together for the additional session stream.

Catalogues arrive as an initial snapshot followed by coalesced changes. The
desktop publishes on occupancy, metadata, mapping or hoist-ownership changes;
ordinary frame commits leave the catalogue unchanged.

Validation: the pairing endpoint test transfers a 256-KiB synthetic payload,
checks conditional replies and live permission denial, and continues control
traffic afterward. The delayed-session test leaves a thumbnail request unanswered
while catalogue exchanges recover, then checks cancellation on owner drop.
Policy tests cover coalescing, refresh intervals, dirty-during-capture state,
cache limits, removal, revocation, expired tickets and malformed wire bounds.

## Current limits

Enrollment and trust policy run on the shared authority worker. Existing identity
keys, pairing links and `devices.json` records keep their format. Revocation
retires live permits before saving; a save failure is reported explicitly and
the same revoke command can retry persistence while access remains disabled.

Session reports and explicit permission-scoped peer collection are described in
[Session diagnostics](session-diagnostics.md). Diagnostics permission is separate
from browsing/hoisting and defaults to disabled.

The phone presents one root surface at a time; multi-window family presentation,
popups and keyboard/IME are later work. Hoisting uses the ordinary family lifecycle,
so related windows may follow the root even though the phone does not yet display
them. Start with a single-window application. Layer-shell desktop furniture and
already-remote windows are excluded from the catalog.

The paired desktop uses AV1 and an 8 Mbps shared target by default. The current
phone-service budget is separate from launcher bitrate overrides. Codec selection
must be supported by the phone. After a long background suspension or lost
connection, use **Reconnect**; automatic retry is not implemented yet.

Pairing controls are installed by graphical Master, not the headless launcher.
Unavailable control storage or sockets produce a warning while ordinary desktop
operation continues. A legacy Iroh endpoint must use its matching persistent
device directory to share pairing controls.

## Validation evidence

On 2026-10-05, the Pixel 8 Pro completed direct-link enrollment, application
listing, AV1 Foot/htop presentation, return and re-hoist, saved-trust reconnect
after restarting both ends, and live revocation followed by rejected reconnect.
The native confirmation dialog appeared for an incoming link; malformed links
were rejected. Google scanner activity launch was observed; physically scanning
the desktop QR remains a manual check.

The focused suites passed: Iroh (71 tests), hoist lifecycle (22), relay core (49),
control socket (2), distribution (24), Android ARM64 tests on the phone (7), and
launcher tests (2). Desktop and Android Clippy checks passed. Fable approved the
security/lifecycle corrections. Existing nested Vulkan acquire-fence validation
messages and the phone's GLES non-AFBC diagnostic still occur; this slice does
not claim to resolve those renderer diagnostics.
