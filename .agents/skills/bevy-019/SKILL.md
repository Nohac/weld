---
name: bevy-019
description: Implement, review, or migrate Weld code against Bevy 0.19 specifically. Use for Bevy ECS, schedules, events, reflection, scenes, BSN, UI primitives, feature flags, or examples where adjacent Bevy versions or the full Bevy runtime could be mistaken for Weld's narrower integration.
---

# Work with Bevy 0.19 in Weld

1. Read `CONTRIBUTING.md` before changing code or structure.
2. Confirm uncertain APIs in versioned Bevy 0.19 documentation or the
   `v0.19.1` source tag. Do not copy `main` blindly.
3. Add only the standalone Bevy crate and features required by the current
   slice.
4. Preserve Weld's host boundaries and test policy without a display server
   where practical.
5. Run the narrowest relevant check or test in debug mode.

## Weld boundaries

- Let Smithay and `calloop` own Wayland objects, backend events, and the outer
  event loop. Do not introduce Bevy's application or window loop implicitly.
- Use `bevy_ecs` for owned compositor state, schedules, events, and policy.
  Keep borrowed Smithay resources and event-loop-dependent lifetimes outside
  the ECS world.
- Translate host input into stable, owned ECS-facing data. Translate policy
  output back into typed requests that the host validates and applies.
- Keep ECS `Entity` values process-local. Use project-owned identifiers at IPC,
  persistence, plugin, protocol, and remote boundaries.
- Keep rendering input independently owned or immutable from the policy
  schedule. Rendering must not mutate window-management policy as a side
  effect.
- Do not add the full `bevy` crate, `DefaultPlugins`, Winit integration, or
  Bevy's renderer merely because an upstream example uses them. Discuss a
  concrete need first.

## Version and dependency rules

- Target the Bevy 0.19 release line and write 0.19 syntax.
- Bevy APIs remain pinned to 0.19, but Weld temporarily patches the active
  rendering crates in `vendor/bevy-wgpu30` to use wgpu 30. Read
  `vendor/bevy-wgpu30.upstream` before changing that compatibility layer, and
  remove it when Weld adopts a suitable Bevy release with native wgpu 30 or
  newer support.
- Select features deliberately. Do not inherit a feature set from a full Bevy
  game or editor when Weld only needs a standalone crate.
- Check the 0.18-to-0.19 migration guide when adapting older examples.
- Treat BSN and headless widget APIs as evolving. Hide repeated framework
  details behind small project-owned functions or components.
- Treat `.bsn` assets as future-facing unless Weld deliberately adds and tests
  an asset-loading path; Bevy 0.19's primary BSN path is code-driven.
- Update only the intended dependency and lockfile entries.

## ECS review checklist

- Verify schedule ordering is explicit where host effects depend on it.
- Verify systems operate on owned, testable state rather than raw protocol
  resources.
- Verify externally visible identities do not expose `Entity`.
- Verify events and effects have clear ownership and draining semantics.
- Prefer a focused schedule test for policy and ordering changes.
- Keep the current package intact until a demonstrated boundary justifies a
  new crate.

## Primary references

- [Bevy 0.19 release notes](https://bevy.org/news/bevy-0-19/)
- [Bevy 0.18 to 0.19 migration guide](https://bevy.org/learn/migration-guides/0-18-to-0-19/)
- [Bevy ECS 0.19 API](https://docs.rs/bevy_ecs/0.19.1/bevy_ecs/)
- [Bevy source at v0.19.1](https://github.com/bevyengine/bevy/tree/v0.19.1)
