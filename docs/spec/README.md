# Weld specifications

These documents describe Weld by subject. They replace the original monolithic
idea document, which mixed implemented behavior, design constraints, examples,
and long-range brainstorming in one roadmap.

Every claim uses one of three statuses:

- **Implemented** — behavior that can be verified in this repository.
- **Direction** — a design constraint or intended capability that should shape
  compatible work, but may not be implemented yet.
- **Exploration** — a candidate approach that still needs design or validation.

Direction and Exploration are not implementation checklists. They do not
authorize placeholder crates, APIs, or abstractions before a concrete slice
needs them. When implementation changes the answer, update the relevant spec
and move only the verified part to Implemented.

Detailed current ownership and lifecycle evidence belongs in
[Architecture](../architecture.md). Direct display validation and recovery
evidence belongs in [Direct DRM presentation](../drm-presentation.md). The
subject specs link to those documents instead of duplicating their internals.
Implementation ideas retained from comparisons with related projects live in
[Possible future improvements](../possible-future-improvements.md); that note is
exploratory rather than an additional roadmap.

The [streaming-budget plan](../remote-budgeting-plan.md) records the
bounded implementation sequence for shared budgets and adaptive bitrate.

## Subjects

| Document | Scope |
| --- | --- |
| [Overview](overview.md) | Purpose, goals, and non-goals |
| [Core runtime](core-runtime.md) | Host loop, application lifecycle, and ownership boundaries |
| [Surfaces and input](surfaces-and-input.md) | Wayland surface roles, seats, focus delivery, and input |
| [Window management](window-management.md) | ECS frames, persistence, floating and tiling policy, and compatibility |
| [Rendering](rendering.md) | Composition, buffer import, frame pacing, and display features |
| [Plugins and configuration](plugins-and-configuration.md) | Extension API, reloadable policy, configuration, and IPC |
| [Identity and meshes](identity-and-meshes.md) | Device trust, mesh membership, resource grants, wallet administration, and recovery |
| [Remote hoisting](remote-hoisting.md) | Window-family admission, placeholders, reclaim, launcher federation, and source recovery |
| [Remote protocol](remote-protocol.md) | Handshake, capabilities, transport bindings, surface modes, codecs, and media streams |
| [Remote presentation](remote-presentation.md) | Destination targets, geometry, extent, scaling, quality disclosure, and enhancement |
| [Remote budgeting](remote-budgeting.md) | Media-device admission, reservations, prioritization, resize scheduling, and fairness |
| [Gaming sandbox](gaming-sandbox.md) | Gamescope-inspired isolation, virtual outputs, launchers, and game input |
| [Distributions](distributions.md) | Reusable crates and proposed Weld assemblies |
| [Wayland proxy](wayland-proxy.md) | Hoisting through existing compositors while preserving local window slots |
| [Platform completeness](platform-completeness.md) | Protocol coverage, XWayland, resilience, diagnostics, and validation |
