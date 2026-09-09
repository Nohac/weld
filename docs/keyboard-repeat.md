# Explicit keyboard repeats

Weld carries `KeyboardKeyState::{Pressed, Released, Repeated}` from nested
Winit ingress through the client runtime and all hoist bindings. A repeat is
not another physical press. Mouse buttons retain `ButtonState`. Both hoist
endpoints require protocol revision **5** and must be rebuilt together.

## Ownership and compatibility

Repeat ownership is stable for the whole native seat, selected at startup:

| Mode | Keyboard v10 application | Legacy application / input-method grab |
| --- | --- | --- |
| `client` | Client timer | Client timer |
| `compositor`, legacy `client` | Explicit upstream repeats | Client timer |
| `compositor`, legacy `disabled` | Explicit upstream repeats | No repeat |

Nested hosts default to `compositor`: Winit supplies upstream cadence. DRM
hosts default to `client`: libinput supplies transitions, and this slice adds
no DRM repeat scheduler. Mode never changes on a key press. The application
host's mode must match the input controller; automatic arbitration between
heterogeneous controllers is not implemented.

The legacy default is `client` for compatibility. Network-delayed releases can
let those timers generate unwanted characters. Enable the tested workaround:

```sh
cargo run -- --legacy-key-repeat disabled
WELD_LEGACY_KEY_REPEAT=disabled scripts/run-iroh-hoist
WELD_LEGACY_KEY_REPEAT=disabled scripts/run-network-hoist --host wlp194s0 --client enp197s0f0u1i1
```

`--legacy-key-repeat client` overrides the environment. Invalid values fail
before opening a host. The setting matters on the instance hosting the Wayland
application; these launchers pass the environment to both test instances.
It affects legacy recipients in `compositor` mode, not native `client` mode.
Selecting `disabled` in `client` mode emits a warning rather than silently
appearing to enable the workaround.
No fake release/press emulation is used.
Keyboard versions below 4 have no `repeat_info` event, so Weld cannot configure
their client timers with this workaround. The version diagnostic makes that
limitation visible; the legacy policy table assumes version 4 or newer.

Use `--keyboard-repeat-mode client` if the parent supplies no cadence, or when
a source is driven by a DRM receiver without a repeat scheduler. A DRM source
driven exclusively by a repeating nested receiver can explicitly select
`--keyboard-repeat-mode compositor`, but that also affects local input: local
DRM input then cannot repeat for v10 clients. This is not mixed-controller
arbitration.

Winit exposes no accessor for its parent's repeat settings. A parent advertising
rate zero provides no repeat cadence. Winit currently binds the legacy Wayland
seat path, so outer Weld must retain legacy `client` repeat for an inner Weld
to receive cadence. Selecting `disabled` on the outer removes it. The explicit
client-mode override is the escape hatch; Weld installs no hidden second timer.

`weld-app::input::KeyboardSettings` is reloadable. Replacing `legacy_repeat`
emits a changed typed host command and updates bound legacy keyboards and an
active input-method grab. Immutable cadence ownership is configured through
`WeldAppBuilder` / `HostBuilder`. Smithay retains its normal base rate/delay
(25 keys/second, 200 ms), computes advertised rates per version, and applies the
same policy to late binds. The hardcoded rate-zero diagnostic is removed.

## Routing and lifecycle

- Repeats require an existing delivered press capture. They cannot acquire
  today's route, synthesize a release, or grow a held-key ledger.
- Focus/alias changes invalidate repetition for that hold, even after returning
  to the original target. Its original route remains available for release.
- The hoist relay forwards repeats in order without modifying its release
  ledger. Peer loss emits one release per held key, not one per repeat.
- Core tracks the addressed press context and invalidates it on actual Smithay
  focus changes, including popup grabs. Smithay owns XKB/protocol held state
  and rejects non-repeatable keys.
- Raw filters consume repeats of consumed shortcuts without changing modifier
  sets or retriggering commands. Repeats bypass the Bevy projection buffer but
  still reach clients at input pace.
- Input uses the existing ordered control stream: no frame ACK, codec wait, or
  new worker. The input/video processing-isolation pass remains separate.

Source logs report bound keyboard versions and mode after focus/configuration
changes, without key codes or typed text. XWayland follows its bound keyboard
version; v10 requires an implementation that handles repeated state. Real
XWayland and input-method clients remain unverified on hardware.

## Validation

Tests cover Weld's translation, captured routes, focus/alias lifecycle, relay
cleanup, wire ordering, shortcuts and live settings. They do not re-test
Smithay's implementation. Its enabled library/examples are compile-checked;
optional Anvil/XWayland builds are outside this run.

Manual acceptance is pending: test short taps, held text/backspace, modifiers,
focus-away-and-back, popup grabs, reclaim and disconnect. Check the logged bound
version before attributing repetition to v10. Compare legacy `client` and
`disabled` to validate the compatibility tradeoff.
