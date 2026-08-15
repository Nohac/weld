# Known issues

## Cursor shape can remain stuck after compositor resize

Starting an SSD resize from an edge, moving the pointer away from the window
perpendicular to that edge, and releasing outside the resize area can leave the
compositor cursor showing the active resize shape after the resize has ended.

The cursor is currently resolved across several independently timed sources:
Bevy hover state, compositor interaction overrides, deferred
`WindowInteractionSession` teardown, and the host cursor handoff. The faulty
sequence crosses those ownership boundaries, so it should not be hidden with
another redraw or unconditional icon reset.

A durable fix should make cursor arbitration explicit: each source needs a
defined priority and lifetime, and pointer release, cancellation, focus loss,
and presentation removal must authoritatively invalidate an interaction-owned
cursor before hover or client cursor policy is resolved again.
