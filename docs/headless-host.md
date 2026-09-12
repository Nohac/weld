# Headless application hosting

## Session-host foundation — Implemented

`weldwm --backend headless` runs a Wayland session without a nested window,
physical display, scanout, Bevy application, or compositor render target. This
is distinct from the offscreen-render benchmarks, which do render. `auto`
never selects it.

```sh
cargo run -- --backend headless --wayland-socket weld-server -- foot
```

No visible window is expected yet. This first batch deliberately rejects local
and Iroh hoist flags, screenshots and Bevy remote debugging. It prepares the
source host; it is not yet an end-to-end remote session launcher.

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
never-mapped surfaces remain ineligible, as in the existing hosts.

Vulkan supplies the existing native DMA-BUF import capability when available.
If adapter/device initialization fails, the host warns and serves SHM clients
without advertising linux-dmabuf. SHM still pays the ordinary normalization
copy per commit. Unconsumed leases release without local GPU sampling. GPU
import support alone is not evidence that a hardware codec is available.

The Wayland seat exists, but this assembly has no local input source or focus
policy. Repeat ownership defaults to client timers. The host stays alive when
its launched command or last application exits. SIGINT/SIGTERM closes the
Wayland session; like the existing launcher, it is not a process supervisor and
does not kill arbitrary descendants of a launched script. Apps normally exit
when their Wayland connection closes.

`weld-core::session_host::SessionHost` owns calloop and protocol state. Pre-run
registration accepts ordinary client adapters and readiness sources, and
exposes the existing DMA-BUF capability for codec bindings. It has no dummy
`CompositionHost` implementation. All three native hosts share the same
completion, ingress, effect and resize-service ordering.

## Live headless hoisting — Next batch / agreed policy

The executable's current transport setup blocks for one startup peer. Headless
mode must replace that with live admission while applications keep running.
The Bevy-free hoist/session policy, not desktop layout or placeholder UI, will:

- automatically present the selected session's existing and new windows,
  including related dialogs and popups, only after receiver authorization;
- let the active receiver request logical size, scale and presentation state,
  with application constraints still enforced by the host;
- preserve apps and their last effective size/scale in memory on disconnect,
  release remote input, and suspend unnecessary streaming;
- let an authorized reconnecting receiver supply new preferences without
  restoring startup dimensions first.

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

## Validation — 2026-09-12

- `cargo check -p weld-core -p weldwm -j2` passed.
- `cargo test -p weld-core --features test-support -j2`: 118 tests passed.
  Core's existing test helpers require `test-support`; without it the test
  target fails to compile at existing `SurfaceId::for_test` calls.
- `cargo test -p weldwm --lib -j2`: 10 tests passed.
- `cargo clippy -p weld-core -p weldwm --all-targets --features weld-core/test-support -j2 -- -D warnings`
  and formatting/whitespace checks passed.
- An external C/Wayland SHM diagnostic ran in a private temporary
  `XDG_RUNTIME_DIR`, with inherited display variables removed and an external
  SIGTERM watchdog. GPU-backed startup advertised native imports; forced
  no-Vulkan startup continued without them. Both configured 1100×300 from
  the client's min/max constraints and completed 60 callbacks/60 buffer
  releases in approximately 0.99 seconds. Hosts outlived the client and exited
  normally on SIGTERM, removing their sockets.
- The no-Vulkan remap check advertised 640×480 at 60 Hz and integer scale 2
  for fractional scale 1.5, without constraining the larger window. After
  unmapping and clearing client constraints, the next configure was 0×0
  (client choice), not the startup default.
- A bounded nested/foot screenshot run exited successfully and produced a
  client frame and shell. It still logged the Vulkan acquire-fence validation
  message also present in the earlier user reports; this batch does not fix it.

These checks do not validate hardware DMA-BUF client rendering in the new host,
DRM scanout, remote input, encoded hoisting or reconnect.
