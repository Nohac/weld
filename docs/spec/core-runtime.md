# Core runtime

## Current runtime — Implemented

Smithay and calloop own live Wayland, session, backend, and protocol lifetimes.
The host translates them into owned Weld data, manually advances a Bevy
application, validates typed application effects, and presents the resulting
composition. The exact crate ownership and preparation order are documented in
[Architecture](../architecture.md).

`WeldApp::builder()` selects and prepares a backend before returning a wrapper
around Bevy's real `App`. A distribution can inspect `ActiveBackend`, add
ordinary Bevy plugins, systems, and resources, and then call `run()`. A lower
level Bevy-free `CompositionHost` contract remains available to alternative
application hosts.

## Ownership rules — Direction

- Smithay remains authoritative for protocol validity and objects tied to the
  server thread or event loop.
- Application policy owns windows, presentation choices, placement, focus,
  layout, shell state, and reloadable settings.
- Rendering consumes independently owned state and must not mutate window
  policy as a side effect.
- Plugins exchange stable Weld identifiers and typed requests across host,
  persistence, IPC, and network boundaries.
- Backend events must reach policy without adding a frame of latency or
  re-entering Bevy from a Smithay callback.

The runtime should remain event driven. Input, protocol changes, timers,
animation demand, remote frames, and administrative commands may request work;
an unrestricted game-style update loop must not become the compositor clock.

## Replacement and recovery — Direction

Ordinary setting changes belong in replaceable Bevy resources so systems see
new values without recreating the world. Backend choice, GPU selection, and
other roots may require deeper reinitialization.

A future application-host replacement path should retain the core-owned
Wayland socket and live clients where safe, snapshot durable policy using Weld
identifiers, and rehydrate a new policy world. That is not implemented, and it
must not be approximated by persisting Bevy `Entity` values or native objects.

## Open work — Exploration

- Define stable ordering points only when external plugins need to target them.
- Determine which failures can be isolated inside an application host and
  which require a clean process restart.
- Model complex backend lifecycle and recovery with explicit state machines
  where that is clearer than scattered flags.
