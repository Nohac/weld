# Surfaces and input

## Current surface model — Implemented

Core exports protocol-neutral surface snapshots; `weld-app` represents mapped
application surfaces and popups in Bevy; and `weld-window` optionally claims
them for default client- or server-decorated presentation. Multiple toplevels,
readable subsurfaces above the toplevel root, XDG popups, scaling,
client-decoration move/resize requests, and precise surface picking are
implemented. See
[Architecture](../architecture.md) for lifecycle and geometry details.

New mapped surfaces are not intrinsically default windows. A policy plugin
claims an application role and builds its presentation, allowing another
distribution to replace the default window policy without touching Smithay.

## Window management — Direction

Application windows should share stable policy components for placement,
stacking, workspace membership, output membership, focus, constraints, and
visual state. Distinct Wayland roles such as popups and layer surfaces must not
be disguised as ordinary windows merely to reuse code.

Protocol roles and their surface-tree ownership remain part of this boundary.
Stable frames, persistent vacancy, and floating or tiling policy are specified
separately in [Window management](window-management.md).

When layer shell becomes a concrete slice, Smithay's `LayerMap` should handle
protocol anchors, margins, exclusive zones, and configure state before Weld
projects the committed result into application policy.

## Current input path — Implemented

Nested winit input and DRM/libinput input enter through separate adapters,
then share timestamped, seat-aware raw records, projection, routing, shortcut,
and client-delivery layers. The nested adapter respects the host compositor's
logical key mapping; the direct backend owns its keymap. Cursor-only DRM motion
can update presentation without running a full Bevy composition.

## Seats and devices — Direction

A seat is a logical collection of input capabilities, not a synonym for one
physical keyboard or mouse. Multiple devices may feed one seat; independent
users generally require separate seats. Multiple visible cursors on one seat
are a separate policy feature and must not be inferred merely because multiple
pointers are connected.

Input policy should support click-to-focus, focus-follows-pointer, directional
focus, focus history, grabs, pointer constraints, shortcut inhibition, touch,
tablet input, and global-shortcut protocols without exposing backend-specific
events to plugins. Remote input must stay scoped to its authorized window or
explicit remote seat.

## Open work — Exploration

- Multi-output workspace and focus behavior.
- Multi-seat routing and whether a distribution wants multiple cursors per
  seat.
- Touch parity for client move and resize.
- Stable public layout and focus extension contracts.
