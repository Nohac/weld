# Platform completeness

## Current usable slice — Implemented

Weld runs nested and directly on DRM, accepts multiple native Wayland
xdg-toplevels and XDG popups, supports SHM and the advertised DMA-BUF subset,
handles keyboard, pointer, scrolling, decorations, fractional client scaling,
move/resize requests, screenshots, and structured tracing. The precise current
surface and renderer limits are recorded in [Architecture](../architecture.md).

This is a development compositor, not yet a complete daily-driver platform.

## First usable release — Direction

The first broadly usable Weld release should run reliably in nested and DRM
modes; manage native Wayland and XWayland applications across multiple windows,
workspaces, and outputs; provide floating and tiling policy; and support focus,
move, resize, close, launch, configuration, and administrative inspection. Its
decorations and shell UI should remain ordinary Bevy composition, while damage
and fullscreen presentation avoid unnecessary work. Automated validation must
protect the Smithay, application-policy, and renderer boundaries.

## Wayland and desktop coverage — Direction

Protocol support should be added in coherent user-facing slices. Important
areas include data devices and clipboard, layer shell, activation, relative
pointer and constraints, shortcut and idle inhibition, primary selection, text
input, session lock, output management and power, capture, foreign toplevels,
tablet input, explicit synchronization, color management, content type, and
tearing control. This list is a prioritization input, not a claim that every
protocol belongs in core or must land together.

XWayland should eventually map legacy clients into the same application policy
as native Wayland windows while retaining X11-specific focus, size hints,
override-redirect, clipboard, fullscreen, and grab semantics behind the host
boundary. XWayland is not implemented.

Multi-output work should preserve per-output scale, transform, refresh,
composition target, and camera ownership. Multi-seat work should retain seat
identity from raw input through focus and delivery. Neither capability should
be simulated by global singleton state once its implementation begins.

## Resilience — Direction

Client failure, malformed requests, output hotplug or sleep, VT deactivation,
network loss, encoder failure, and recoverable presentation failure must not
turn routine external events into desktop crashes. Native object loss should
transition through explicit states, stop unsafe work, preserve recoverable
policy, and resume only after capabilities are re-established.

GPU device loss and application-host replacement need separate designs; where
continuation cannot be made safe, Weld should fail clearly and leave enough
external state for an orderly restart rather than continuing with invalid
graphics or protocol ownership.

## Performance constraints — Direction

- Do not redraw continuously while the scene is idle.
- Keep input and protocol processing independent from rendering, presentation,
  and remote encoding.
- Avoid CPU pixel copies for DMA-BUF client content and avoid hidden fallback
  paths that invalidate the selected fast path.
- Bound queues toward the newest useful composition or remote frame.
- Keep shader compilation and blocking codec work off presentation-critical
  paths.
- Make slow interactive resize clients degrade visually without stalling the
  compositor.
- Measure before assigning numeric budgets; diagnostic observations are not
  product guarantees.

## Diagnostics and validation — Direction

Structured tracing should carry stable surface, window, output, seat, peer,
and hoist identifiers where relevant. Useful diagnostics include schedule and
render timing, buffer lifetime, damage visualization, surface trees, direct
scanout rejection, frame latency, and remote bitrate, loss, decode, and queue
metrics.

Validation should combine focused policy tests, deterministic application
schedule tests, nested Wayland integration tests, hardware DRM/VT smoke tests,
render comparisons where they verify shader output, and network simulation for
remote state machines. Tests should protect behavior and ownership contracts,
not merely assert values the code just inserted into Bevy.
