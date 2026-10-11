# Native application launch

`weldctl` can start applications through an existing Weld host:

```sh
weldctl --session weld-0 launch -- foot
weldctl --session weld-0 launch --cwd /path/to/project -- ./target/debug/my-app
```

Inside a Weld-launched terminal, `WAYLAND_DISPLAY` selects the session when
`--session` is omitted. An absolute Wayland socket path is also accepted.
`--socket` selects the administrative control endpoint directly.

The working directory defaults to the caller's current directory. Relative
`--cwd` values are resolved there. Executable arguments retain their exact
boundaries; shell expansion requires explicitly launching a shell.

Weld configures the child's Wayland endpoint, its XWayland `DISPLAY` when
enabled, session IPC endpoints and cursor settings through the same launcher as
config `exec` and startup commands. The application follows ordinary window
admission, placement, persistence matching and focus policy. The response reports
successful process creation and its PID; windows may appear afterward. Apps that
forward requests to an existing instance may open on that instance's display.

The control endpoint is local administrative authority, protected by a private
directory and same-user credentials. Pairing and network listeners remain lazy;
launching works in headless hosts and receiver-only sessions too. Window-scoped
agent grants, attributed launch sockets and peek follow the contracts in
[Window access](spec/window-access.md).

The native request queue holds at most 32 launches and services eight per host
turn. Requests expire after five seconds before dispatch. Spawn failures return
to the caller while the compositor continues. If a reply is lost after dispatch,
the outcome can be ambiguous; callers should inspect before retrying.

## Validation

Run from the normal development environment:

```sh
scripts/check-control-launch
scripts/check-control-launch --nested --xwayland
```

`--no-build` reuses existing binaries and probe examples; Cargo still supplies
the host's debug dynamic-library environment. The headless test forces SHM-only
hosting and checks native configure, releases and dormant callback behavior.
The nested test checks Foot admission, and `--xwayland` additionally launches
the X11 probe through the same RPC. Both check argument/cwd preservation, missing
executable/directory errors, subsequent successful launches and control socket cleanup.
Logs live under `target/validation/control-launch-*`.

Both runs passed on 2026-10-11. Direct DRM uses the same launch queue and child
owner; manual validation is to launch Foot from another terminal against the
running DRM session and confirm it appears on the intended workspace.

The XWayland run also exposed stale Wayland socket files after shutdown, matching
the earlier `target/validation/xwayland-ol_pe2cv` run. The runner reports this
host-lifecycle issue explicitly and verifies that the endpoint no longer accepts
connections. XWayland callbacks retain event-loop handles; source teardown needs
a separate audit. The native control endpoint is removed in both modes.
