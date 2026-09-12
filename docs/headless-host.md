# Headless application hosting

Headless is now an entrypoint assembly of the common native host, not a separate
runtime. See the [shared native runtime plan](native-host-runtime-plan.md) for
remaining output-demand and reconnect work.

## Session-host foundation — Implemented

`weldwm --backend headless` runs a Wayland session without a nested window,
physical display, scanout, Bevy application, or compositor render target. This
is distinct from the offscreen-render benchmarks, which do render. `auto`
never selects it.

```sh
cargo run -- --backend headless --wayland-socket weld-server -- foot
```

No visible window is expected from that command alone. Local hoist transports,
Iroh destination mode, screenshots and Bevy remote debugging are rejected. An
Iroh source is supported with explicit whole-session consent, as described below.

Defaults are a 1920×1080 physical virtual output, scale 1, a 60 Hz virtual frame
opportunity, and 960×640 logical initial toplevels. Configure them independently:

```sh
cargo run -- --backend headless --wayland-socket weld-server \
  --headless-output 2560x1440 --scale 1.5 --headless-refresh 90 \
  --headless-window-size 1280x720 -- foot
```

Initial dimensions pass through the same committed application min/max
constraints as ordinary resize requests, before the first XDG configure. They
are consumed once per toplevel: remapping does not reapply the startup default.
The host does not continuously enforce a window size. Logical window size,
application rendering scale and encoded media resolution remain distinct.

The finite virtual output is Wayland compatibility information, not a workspace
boundary: a window can be larger than it, and the session does not clamp windows
into a desktop layout. An unbounded workspace does not require advertising an
enormous monitor or allocating an enormous framebuffer. Native buffer and codec
limits still apply to individual surfaces independently of that workspace.

Mapped surfaces receive frame callbacks at no more than the configured 1–240 Hz
rate, without catch-up bursts after delays. This is an opportunity to produce
another frame, not confirmation that a remote screen displayed it. No new
display-feedback or network-ACK semantics are introduced. Without pending
mapped callbacks, calloop waits on its sources with a one-second child-reaping
maintenance timeout, rather than waking at the refresh rate. Callbacks on
currently unmapped surfaces remain ineligible, as in the existing hosts.

Vulkan supplies the existing native DMA-BUF import capability when available.
If adapter/device initialization fails, the host warns and serves SHM clients
without advertising linux-dmabuf. SHM still pays the ordinary normalization
copy per commit. Unconsumed leases release without local GPU sampling. GPU
import support alone is not evidence that a hardware codec is available.

The Wayland seat exists, but this assembly has no local input source or focus
policy. Without a remote receiver, repeat ownership defaults to client timers.
An Iroh source defaults to compositor-owned repeats supplied by the receiver;
explicit CLI settings still win. The host stays alive when
its launched command or last application exits. SIGINT/SIGTERM closes the
Wayland session; like the existing launcher, it is not a process supervisor and
does not kill arbitrary descendants of a launched script. Apps normally exit
when their Wayland connection closes.

`weld-core::runtime::HostRuntime` prepares the common runtime without a native
presenter. Nested and DRM use that same runtime loop and completion ledger. Pre-run
registration accepts ordinary client adapters and readiness sources, and
exposes the existing DMA-BUF capability for codec bindings. It has no dummy
`CompositionHost` implementation. Optional policy and composition borrow one
integration owner through `ApplicationHost`; `HostRuntime::with_policy` can run
policy without rendering at all. Unsupported capture returns an explicit error.
Native drivers retain Winit/DRM timing and native effects, including cursor and
VT ordering. GPU bootstrap shares device/import setup without changing each
driver's adapter-selection requirements.

Presenter-free policy advances initially, on client events, or while its
`advance_main` return requests another paced update. Returning false with no
client traffic lets it sleep; there is no arbitrary-deadline API or implicit
Bevy startup-settle sequence. Adapter timers/readiness remain serviced by
calloop independently. Future policy-owned retention/reclaim timers must either
request continued updates or add an explicit deadline contract.

## Live headless Iroh demo

```sh
scripts/run-headless-iroh-hoist
# A small, bounded run using the currently validated codec:
scripts/run-headless-iroh-hoist --codec h264 --app foot --seconds 15
```

The launcher starts foot running htop, Blender and a private-profile Firefox in
a new, presentation-free host. A nested Weld receiver opens on your existing
desktop. All mapped toplevels auto-hoist, including future dialogs; popups stay
in their owner's session. No source desktop, placeholder UI or Super+H is needed.
Use repeated `--app` options to select fewer applications.

AV1 and direct Iroh are the defaults (`--codec h264` is also supported).
**Hardware warning:** the initial AV1 run on this Radeon system triggered a
VCN video-engine timeout/reset, followed by an invalid AV1 packet and loss of
the source GPU context. Another GPU client was affected too. The trigger remains
unresolved; the subsequent H.264 foot/htop run succeeded. Use `--codec h264`
for the currently validated path. The launcher prints this warning before starting AV1.
`--network n0` explicitly enables Internet discovery/relay services; this launcher
does not isolate interfaces or change routes. Both endpoints otherwise run in
the current network namespace. The default has **no runtime timer**. `--seconds`
adds one after receiver startup; `--ticket-timeout` separately bounds pairing.
`--receiver-delay` is a diagnostic to exercise hosting before pairing completes.

Invoking the launcher authorizes every existing and future window in its new,
isolated session; it starts without an interactive prompt. The source CLI separately
requires `--backend headless --hoist-all` with `--hoist-iroh-listen` and
`--hoist-iroh-expect-peer`. Each run uses fresh private ticket/identity files;
mutual Iroh identity authorization and the existing codec handshake precede any
surface metadata, pixels or input. This is one peer, not unattended discovery.

Source preparation installs a pending port before running the common host.
The trusted identity-file wait and peer admission are asynchronous, cancellable
on drop, and share one timeout. Until authorization, the relay retains only
latest surface state/leases, not a queue of historical frames. On readiness it
replays static windows even if they never commit again. Actual encoder sessions
are allocated only when encoding begins. Source capability checks require the
encoder plus VPP; receiver checks require the decoder plus VPP, independently.

Receiver size/scale/input requests use the existing client relay and native
application constraints. Repeat ownership defaults to `compositor` on this
source; the launcher also selects `--legacy-key-repeat emulated` for clients
such as foot. See [keyboard repeat policy](keyboard-repeat.md).

Ctrl+C or closing either host stops **this run's** process groups and demo apps,
including descendants left after Weld exits. Supervisors pin those group IDs
until cleanup and also stop them if the launcher disappears. Core dumps are
disabled. Logs, process IDs and the private Firefox profile remain under the
printed `target/validation/headless-iroh-*` directory. Existing desktop apps and
profiles are untouched. There is no automatic reconnect in this slice.

## Retention and reconnect — Follow-up

Source construction is now shared across the Unix, connected Iroh and pending
Iroh paths. `EncodedSourcePort::configured` owns budget/dump configuration and
disconnects the supplied transport if setup fails. Native Iroh paths use one
`IrohSourceRegistrationOptions` and backend/wake factory; only the manual
endpoint needs a destination namespace. This removes duplicated setup, not the
remaining pacing difference between presentation-driven and virtual callbacks.

The next policy slice should preserve apps and their last effective size/scale
in memory on disconnect, release remote input, suspend unnecessary streaming,
and let an authorized reconnecting receiver supply new preferences without
restoring startup dimensions first. Currently the standalone source remains
alive after peer failure, but the demo launcher stops it when the receiver exits.

Retention needs an explicit relay policy: the existing source relay resets
preferred scale on withdrawal or failure. Do not remove desktop restoration
behavior globally. There are no local placeholders to restore in headless mode.
Admission and snapshot replay must also handle static windows that have not
committed since the receiver connected.

Startup defaults apply only before receiver preferences exist. Codec budget
changes must not implicitly reconfigure application size or scale. Future
multi-viewer sessions need one designated presentation owner per window;
additional viewers can scale/crop locally. Presentation ownership and input
ownership are separate. Persistence across host restarts is outside this slice.

Maximize/fullscreen policy should fill the active receiver's presentation area,
not the headless virtual output or an infinite workspace. That receiver-driven
policy is not implemented by this foundation. Non-Wayland adapters may consume
presentation preferences directly without inventing an output/monitor object.

See the [portable receiver follow-ups](receiver-decoder-pool.md#portable-execution-boundary)
for the phone/Godot and OpenXR sequence.

## Native-runtime foundation validation — 2026-09-12

`scripts/check-host-runtime --policy-only --shm-only` also exercises a non-Bevy
policy owner with no native presenter: events and main advances are serviced,
no rendering occurs, and a capture request fails explicitly.

- `cargo check -p weld-core -p weldwm -j2` passed.
- `cargo test -p weld-core --features test-support -j2`: 122 tests passed.
  Core's existing test helpers require `test-support`; without it the test
  target fails to compile at existing `SurfaceId::for_test` calls.
- `cargo test -p weldwm --lib -j2`: 10 tests passed.
- `cargo clippy -p weld-core -p weldwm --all-targets --features weld-core/test-support -j2 -- -D warnings`
  and formatting/whitespace checks passed.
- The tracked `scripts/check-host-runtime` subprocess fixture runs in a private
  `XDG_RUNTIME_DIR`, with inherited display variables removed and an external
  timeout. Run it normally and with `--shm-only`. GPU-backed startup advertised
  native imports; forced no-Vulkan startup continued without them. Both configured
  1100×300 from the client's min/max constraints and completed 60 callbacks/60 buffer
  releases in approximately 0.99 seconds. Hosts outlived the client and exited
  normally on SIGTERM, removing their sockets.
- The no-Vulkan remap check advertised 640×480 at 60 Hz and integer scale 2
  for fractional scale 1.5, without constraining the larger window. After
  unmapping and clearing client constraints, the next configure was 0×0
  (client choice), not the startup default.
- `scripts/check-host-runtime --nested` checks a bounded nested/foot screenshot
  (explicitly skipped without a parent display or foot). The baseline run exited
  successfully and produced a client frame and shell. It still logged the Vulkan
  acquire-fence validation message also present in the earlier user reports;
  this batch does not fix it.

These checks do not validate hardware DMA-BUF client rendering in the new host,
DRM scanout, remote input, encoded hoisting or reconnect.

## Headless Iroh launcher validation — 2026-09-12

- Hoist-core's 34 tests cover mapped-only admission, separate dialog sessions,
  owner-session popups, exactly-once replay, checked IDs, manual-command rejection
  and retaining only the latest buffer use while waiting.
- Iroh's 44 portable tests include asynchronous pairing, timeout, exclusive
  admission, cancellation during handshake, abandoned ready-peer cleanup,
  pre-ready port isolation and failure while configuring an admitted port.
- VA-API's 8 policy tests and the distribution's 12 tests passed, including
  independent source/receiver capability gates and explicit session consent.
- Core's 122 existing tests passed. Both `scripts/check-host-runtime --shm-only`
  and `--policy-only --shm-only` passed again: 60 callbacks/releases in about
  0.99 seconds, remap behavior, host lifetime, signal shutdown and socket removal.
- `python3 scripts/test-headless-iroh-launcher.py` uses subprocess fixtures, not
  GPU/network work, for process ownership, inherited signal masks, Ctrl+C during
  startup/running, and receiver exit/failure cleanup.
- Live H.264 foot/htop over direct Iroh with a two-second receiver delay paired,
  auto-mapped, and accepted user input. The source logged `repeat_mode=Compositor`
  and `legacy_repeat=Emulated` for foot's keyboard v8. Logs:
  `target/validation/headless-iroh-0wdwace4`.
- The preceding AV1 attempt **failed** with the VCN reset described above:
  `target/validation/headless-iroh-7mx14q11`. Hardware testing was paused;
  three-app live validation, a repeat AV1 test, and native GPU/nested regression
  reruns are not claimed for this batch. Neither DRM nor real-network/5G testing
  nor reconnect was performed.

The AV1 investigation should compare a cached first frame with a live first
frame, and delayed pairing with delayed application startup, before changing
the codec pipeline. The existing `--hoist-encoded-dump-dir` source option can
separate source bitstream corruption from transport or presentation. Lease
retention and admission ordering checks have not identified a cause; a kernel
timeout alone does not prove a driver-only defect.

## Source assembly unification — First batch

- The common constructor is used by both Unix source factories and by ready
  and pending Iroh registration. Existing Unix dump availability is preserved;
  no new transport flag, codec setting or wire message is introduced.
- Portable tests compare ready/pending first-frame traffic (normalizing only
  timestamps and independent-stream arrival order), enforce budget application
  before the first frame, and check transport closure on configuration errors.
- A fake encoder with explicitly retained input leases progresses identically
  with no presentation consumer and with one holding every input lease. Final
  buffer release still waits for all consumers. This tests the ownership
  contract, not real GPU synchronization or performance.
- `scripts/check-host-runtime --shm-only --frame-timings` records separate
  producer-clock delays for each of 60 callbacks and `wl_buffer.release` events.
  Run `host-runtime-_1lbjv51` completed 60 frames in 988 ms: after startup,
  callbacks were roughly 16.6–17.3 ms after commit, while SHM releases were
  roughly 0.2–0.85 ms after commit. These are SHM-copy releases, not DMA-BUF
  encoder completion, and no encoder ran during this probe.

The renderer-present/absent runtime comparison and any callback-ownership
change remain the next batch. In particular, moving the working nested path to
the current headless clock is not accepted as a latency fix without evidence.
The repeated headless AV1 VCN resets and H.264 input-to-frame lag remain open.
