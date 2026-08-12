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

## Subjects

| Document | Scope |
| --- | --- |
| [Overview](overview.md) | Purpose, goals, and non-goals |
| [Core runtime](core-runtime.md) | Host loop, application lifecycle, and ownership boundaries |
| [Surfaces and input](surfaces-and-input.md) | Wayland surface roles, seats, focus delivery, and input |
| [Window management](window-management.md) | ECS frames, persistence, floating and tiling policy, and compatibility |
| [Rendering](rendering.md) | Composition, buffer import, frame pacing, and display features |
| [Plugins and configuration](plugins-and-configuration.md) | Extension API, reloadable policy, configuration, and IPC |
| [Remote hoisting](remote-hoisting.md) | Individual and grouped window hoisting, adaptive media, reclaim, and security |
| [Gaming sandbox](gaming-sandbox.md) | Gamescope-inspired isolation, virtual outputs, launchers, and game input |
| [Distributions](distributions.md) | Reusable crates and proposed Weld assemblies |
| [Platform completeness](platform-completeness.md) | Protocol coverage, XWayland, resilience, diagnostics, and validation |
