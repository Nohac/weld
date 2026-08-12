# Gaming sandbox

## Status — Exploration

The gaming sandbox is a proposed Weld plugin that acts as a focused nested
micro-compositor for one game or launcher group. It is not implemented, and it
must remain outside `weld-core` until a concrete slice proves the required
protocol and process boundaries.

Every section below remains Exploration until explicitly promoted.

## Gamescope inspiration and compatibility

[Valve Gamescope](https://github.com/ValveSoftware/gamescope) describes itself
upstream as a gaming micro-compositor that can run nested or directly on a VT
and expose a virtual display with a chosen resolution and refresh rate. It is
both design inspiration and a compatibility target for Weld's sandbox.

Compatibility initially means matching explicitly selected user-visible
behaviors and launch workflows—not binary, CLI, private-protocol, or drop-in
compatibility. Each claimed behavior needs a fixture and comparison against a
documented Gamescope invocation. Candidate behaviors include:

- presenting a stable virtual output mode to the contained client;
- hosting a game or launcher in a focused nested compositor;
- scaling that fixed game surface into an independently sized destination;
- correct fullscreen, relative-pointer, and cursor-confinement behavior; and
- integration points expected by a future streaming plugin.

## Fixed virtual display

The sandbox should advertise a controlled `wl_output` mode, such as 1920×1080,
that does not change merely because the local or remote presentation is
resized. Games see a stable display environment while Weld scales or places the
sandbox output elsewhere. Mode, refresh, scale, HDR, and VRR exposure require
explicit policy; an “unchangeable resolution” must not prevent a deliberate
configuration change or game-requested mode transition that the plugin elects
to support.

## Launcher and game handoff

A sandbox session should survive a launcher replacing its login window with a
different game process or application ID. The stream follows the sandbox's
selected primary presentation rather than one original `wl_surface`.

Process ancestry and application IDs are useful evidence but not sufficient
authority by themselves. The design must account for launchers, pressure-vessel
or Proton containers, helper processes, multiple windows, crashes, and explicit
user selection without letting unrelated clients enter the sandbox.

The sandbox determines which windows belong to its contained application and
which presentation is primary. Authenticated remote discovery and launch are
owned by [launcher federation](remote-hoisting.md#launcher-federation--direction).

## Native game input

Wayland relative-pointer and pointer-constraint protocols should provide
locked 1:1 camera input inside the virtual output. Focus loss, disconnect, and
reclaim must release constraints and synthesize the required key or button
releases so a game cannot retain stuck input state.

Remote controller input may be exposed through a narrowly scoped Linux
`uinput` device so games see a conventional gamepad. Device creation is a
privileged capability with explicit ownership, teardown, permission, and
session isolation; it must not become general remote host input.

## Relationship to hoisting

The sandbox owns containment, virtual output policy, and selection of the
primary game presentation. [Remote hoisting](remote-hoisting.md) owns peer
trust, streams, media adaptation, and reclaim. A local-only sandbox must remain
useful without networking, and the networking layer must not depend on
game-specific policy. Remote admission uses the same separately selected scope
and admission-mode axes as ordinary hoisting; sandbox membership alone must
not silently grant a peer access.
