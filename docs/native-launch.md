# Native application launch

`weldctl` can start applications through an existing Weld host:

```sh
weldctl --session weld-0 run -- foot
weldctl --session weld-0 run --cwd /path/to/project -- ./target/debug/my-app
weldctl run --workspace current -- rofi -show drun
weldctl run --workspace 3 -- foot
```

`run` and `exec` are visible aliases for `launch`. Inside an attributed launch,
`WELD_SESSION` selects the parent session; ordinary terminals fall back to
`WAYLAND_DISPLAY`. `--session` takes precedence. An absolute socket path is accepted.
`--socket` selects the administrative control endpoint directly.

Use matching host and `weldctl` builds. The attributed launch request has its own
RPC discriminator, so older hosts reject it before spawning rather than silently
ignoring headless/control options.

The working directory defaults to the caller's current directory. Relative
`--cwd` values are resolved there. Executable arguments retain their exact
boundaries; shell expansion requires explicitly launching a shell.

Each launch gets an owner-only Wayland socket admitted directly into the host's
existing display. Accepted connections carry a host-local launch ID. Children
inherit that endpoint, so a launcher such as Rofi can exit before its application
connects. `--workspace current` captures the workspace before spawning; explicit
destinations survive later workspace switches and do not steal focus.

The JSON receipt contains `process_id` and an eight-character `launch_id`.
It confirms process creation; application readiness follows asynchronously.
`window find --launch-id ID --json` finds the resulting windows, and `window wait`
waits for inventory changes. An application's own readiness signal remains useful
before sending input—for example, a terminal may map before its child program starts.

For ordinary X11 launches, use `run --shared-display -- steam`. This exposes the
host's shared Wayland/XWayland endpoints and returns a null launch ID. Shared-display
launches reject workspace, owner and automatic-control options. Applications that
delegate to an already-running process keep that process's existing connection;
use their separate-instance/profile options when launch attribution is needed.

## Controlled and headless applications

```sh
weldctl run --control --owner agent --headless --cwd /path/to/project -- ./my-app
weldctl window find --owner agent --json
weldctl window peek --control CONTROL_ID
weldctl window peek --control CONTROL_ID --visible false
```

`--control` grants View, Keyboard, Pointer and Resize to windows arriving through
that launch endpoint. `--owner` is a searchable label. MCP's `window_list` and
`window_find` accept owner and launch-ID filters. Existing input, resize, scaling,
capture and accessibility operations use the returned window's control ID.

`--headless` requires `--control` and keeps new windows outside desktop layouts.
They retain independent viewport sizing and input through the shared control/hoist
path. `peek` shows the application in the current workspace with the normal control
header. Its local frame clips the independent application viewport. Closing or
hiding that peek removes the local view while the application and control ID survive.
Repeated peek/hide operations preserve the viewport.

Local frames start hidden and become drawable after window/output reconciliation.
Removing their output assignment or losing output geometry hides the complete
presentation, including its decorations and surface content.

A matching persistent slot retains its placement and becomes the local peek,
including for a headless launch. Its existing close-and-placeholder behavior applies.
Manual permission changes remain effective; automatic admission runs once per
occupant. `weldctl access revoke --all` revokes active grants and the launch's ability
to grant access to later windows, including after new sessions are allowed again.

The control endpoint is local administrative authority, protected by a private
directory and same-user credentials. Pairing and network listeners remain lazy;
ordinary attributed launching works in presentation-free hosts and receiver-only
sessions too. Workspace placement and automatic control require the window-managed
distribution. Attribution authenticates the listening endpoint, within same-user
administrative authority; it does not isolate processes from other same-user tools.

Launch sockets and policies last until the host session ends so delayed descendants
can connect. Each session accepts up to 256 attributed launch contexts. Failed
native spawns remove their socket immediately and retire their prepared policy.
An ambiguous host-reply timeout retains attribution for inspection. Host shutdown
removes the launch listeners. Exact process-tree supervision and explicit early
retirement are future lifecycle work.

The native request queue holds at most 32 launches and services eight per host
turn. Requests expire after five seconds before dispatch. Spawn failures return
to the caller while the compositor continues. If a reply is lost after dispatch,
the outcome can be ambiguous; callers should inspect before retrying.

## Validation

Run from the normal development environment:

```sh
scripts/check-control-launch
scripts/check-control-launch --nested --xwayland
scripts/check-control-launch --nested --attributed
```

`--no-build` reuses existing binaries and probe examples. Run in the shared
development shell for native graphics and media libraries. The headless test forces SHM-only
hosting and checks native configure, releases and dormant callback behavior.
The nested test checks Foot admission, and `--xwayland` additionally launches
the X11 probe through the same RPC. Both check argument/cwd preservation, missing
executable/directory errors, subsequent successful launches and control socket cleanup.
Logs live under `target/validation/control-launch-*`.

The attributed test exercises a launcher exiting before its child connects,
background-workspace placement, owner/launch filtering, automatic headless control,
keyboard delivery, resize, pixel capture, repeat peek/dismiss and global revocation.
The launch/UI regression also checks actual frame visibility for client- and
server-decorated windows through initial headless admission, repeated peek/close,
and output-geometry loss/restoration.
Policy tests additionally cover switching away after capturing `workspace current`,
multiple connections, and keeping a revoked launch disabled for delayed windows.

Both runs passed on 2026-10-11. Direct DRM uses the same launch queue and child
owner; manual validation is to launch Foot from another terminal against the
running DRM session and confirm it appears on the intended workspace.

The XWayland run also exposed stale Wayland socket files after shutdown, matching
the earlier `target/validation/xwayland-ol_pe2cv` run. The runner reports this
host-lifecycle issue explicitly and verifies that the endpoint no longer accepts
connections. XWayland callbacks retain event-loop handles; source teardown needs
a separate audit. The native control endpoint is removed in both modes.
