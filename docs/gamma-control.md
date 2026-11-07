# Output gamma control

Weld's physical DRM backend implements `wlr-gamma-control-unstable-v1`, used by
Gammastep for blue-light reduction. Nested and headless hosts do not advertise
the protocol: the parent compositor controls the physical display's gamma.
Protocol fixtures can explicitly enable mock outputs for testing.

Run Gammastep inside the physical Weld session, for example:

```sh
gammastep -m wayland -l 59.9:10.7 -t 6500:4500
```

Replace the example latitude/longitude with your location, or use your normal
Gammastep configuration. For a constant temperature without transitions:

```sh
gammastep -m wayland -l 0:0 -t 4500:4500 -r
```

Stopping the client restores the original output tables. Only one gamma
controller can own each output; competing clients receive the protocol's
`failed` event. No Weld configuration toggle or Bevy effect is needed.

## Ownership and lifecycle

The Wayland host validates each submitted file as exactly three consecutive
native-endian `u16` arrays with the hardware-advertised length. Pipes, devices,
truncated files and incorrectly sized files are rejected with `invalid_gamma`.
Reads use a bounded copy at offset zero, independent of the client's shared file
offset. Hardware sizes outside 1–65,536 entries are unsupported.

Updates coalesce per output. An unchanged table causes no hardware update, and
gamma requests do not schedule a Bevy redraw. The DRM adapter applies the latest
table when the session is active and that output has no pending page flip. It
uses `GAMMA_LUT`/`GAMMA_LUT_SIZE` with a tested, blocking color-only atomic commit;
drivers without these properties can use legacy gamma ioctls. Color-only commits
request no page-flip event, so they cannot consume a frame's retirement callback.

The adapter saves the initial hardware table, including a non-linear table or
an unset atomic LUT. Controller destruction or client disconnect schedules its
restoration. Pause and clean shutdown attempt restoration before releasing the
device. Resume reapplies the retained desired table. Device-access failures
during pause can prevent restoration and are logged.

An apply failure invalidates only that output's controller and capability, with
a best-effort restoration of its original table. A quarantined output similarly
invalidates its controller. Restart is required to rediscover capabilities;
dynamic connector hotplug remains deferred with the rest of the DRM backend.

This is an output hardware transform, after composition. Screenshots, recordings
and streams keep their original colors. Cursor-plane treatment depends on the
driver's color pipeline. This does not implement ICC profiles, HDR, or general
Wayland color management; future color transforms must share ownership of the
hardware pipeline instead of independently overwriting these properties.

## Validation

The focused tests send real Wayland requests into mock physical outputs and run
the installed Gammastep client against that isolated socket:

```sh
cargo test -p weldwm -p weld-core --lib gamma_fd_validation
cargo test -p weldwm -p weld-core --lib gamma_tests -- --ignored --test-threads=1
```

The second command requires local sockets, `XDG_RUNTIME_DIR`, and `gammastep` in
`PATH`; it never changes real display tables. Coverage includes independent
output sizes, exclusive ownership, update coalescing, manager destruction,
controller destruction, disconnect, invalid-file protocol errors, resume replay,
backend failure and stale controllers after output replacement.

Physical validation requires a real DRM session: run the constant-temperature
command, check all supported outputs, stop it and check restoration, then repeat
across VT switching/suspend and clean compositor shutdown. The implementation
environment exposed no `/dev/dri` device, so these hardware checks remain
unverified. Do not run a second DRM compositor over an active desktop to test it.
