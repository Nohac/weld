# Access sessions

The running Weld host keeps its access authority in memory. Each source hoist
connection carries a revocable permit. Revoking it stops admission and queued
ordinary input, releases held input, withdraws presentation claims and disconnects
the transport. Existing hoist policy restores local presentation.

Use the native control client against the running session:

```sh
weldctl --session weld-0 access status
weldctl --session weld-0 access revoke --session 1
weldctl --session weld-0 access revoke --all
weldctl --session weld-0 access allow
```

The outer `--session` selects the Weld instance. The revoke command's
`--session` selects an access-session ID printed by `status`.

Scoped revocation blocks fresh connections from that participant for the
remaining host lifetime until explicit `allow`. Global revocation blocks all
new admission. Allow admits fresh sessions under existing local/paired-device
approvals; retired permits remain invalid. Fixed development-launcher endpoints
need a new run after revocation. Paired device sessions can reconnect.

Master supports this emergency binding, included in the example configuration:

```sway
bindsym $mod+Control+Escape weld access revoke all
```

Restarting Weld starts a fresh authority. Device pairing retains its existing
storage and consent behavior. Per-window local agent controls are the next layer
described in [Window access](spec/window-access.md).

## Validation

Focused authority and relay tests cover admission blocking, fresh-session
reopening, authority teardown, queued-input rejection, held-key cleanup and
revocation before automatic source admission. The local control test also blocks
a launch request while verifying revocation completes independently.

```sh
cargo test -p weld-access -p weld-hoist-core -p weld-control --lib
```
