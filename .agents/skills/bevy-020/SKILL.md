---
name: bevy-020
description: Implement, review, or migrate Weld's Bevy integration to 0.20. Use for Bevy ECS, scheduling, extraction, rendering, scenes, UI, feature selection, and upstream examples while preserving Weld's host and presentation boundaries.
---

# Work with Bevy 0.20 in Weld

1. Read `CONTRIBUTING.md` before changing code or structure.
2. Check the workspace manifests and lockfile for the version actually being
   built. This skill prepares the 0.20 migration; its installation does not
   upgrade the repository's existing 0.19.1 dependencies.
3. Confirm target APIs in versioned 0.20.0 documentation and the `v0.20.0`
   source tag. For maintenance before migration, use the resolved 0.19.1 API.
4. Add only the standalone Bevy crate and features required by the current
   slice.
5. Preserve Weld's host boundaries and test policy without a display server
   where practical.
6. Run the narrowest relevant check or test in debug mode, following the shared
   desktop feature graph in `CONTRIBUTING.md`.

Read [the migration audit](references/migration-020.md) before upgrading
dependencies or changing extraction, surface rendering, or the vendor patches.
For scene authoring, also read `bevy-bsn-ui`.

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
- Assemble plugins explicitly at the application boundary. Upstream examples
  demonstrate their APIs using their own runner; adapt the example to Weld's
  existing host and redraw ownership.

## Version and dependency rules

- Bevy 0.20.0 uses wgpu 30. Audit the additional local patches described in the
  migration reference before removing `vendor/bevy-wgpu30`; native wgpu support
  resolves only the dependency compatibility part.
- Select features deliberately. Do not inherit a feature set from a full Bevy
  game or editor when Weld only needs a standalone crate.
- Keep workspace Bevy versions and features centralized. Also inspect separate
  application workspaces before deleting a shared vendor directory.
- Treat BSN and headless widget APIs as evolving. Hide repeated framework
  details behind small project-owned functions or components.
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

## Scheduling and performance

- Use strict ordering for externally observable sequences: buffer publication,
  host requests, queue submission, and completion handling. Bevy's new weak
  ordering tracks ECS access conflicts; channels, atomics, GPU queues, and
  interior-mutability dependencies need explicit edges.
- Consider `chain_weak`, `before_weak`, or `after_weak` for measured scheduling
  bottlenecks whose dependencies are fully expressed through ECS accesses.
  Review upstream weak render-set ordering when inserting Weld systems.
- Consider `#[component(summary_tick)]` for frequently queried, rarely changed
  components after measurement; the extra mutation bookkeeping has a cost.
- Use seeded schedule randomization for ordering investigations when the
  required Bevy debug feature is enabled. Record the seed and preserve the
  ordinary build graph outside that explicit diagnostic variant.
- Retained UI requires correct invalidation and removal handling. Review
  Weld's custom extraction hooks against that lifecycle before relying on its
  CPU savings; measure both static chrome and content-only frames.
- The `low_power` example's `WinitSettings` controls a Winit runner. In Weld's
  compositor, apply the wake/redraw principles through the existing host loop.

## Primary references

- [Bevy 0.20 release notes](https://bevy.org/news/bevy-0-20/)
- [0.19 to 0.20 migration guide](https://bevy.org/learn/migration-guides/0-19-to-0-20/)
- [Bevy ECS 0.20 API](https://docs.rs/bevy_ecs/0.20.0/bevy_ecs/)
- [Scheduling source](https://github.com/bevyengine/bevy/blob/v0.20.0/crates/bevy_ecs/src/schedule/config.rs)
- [Low-power example](https://github.com/bevyengine/bevy/blob/v0.20.0/examples/window/low_power.rs)
- [Bevy source at v0.20.0](https://github.com/bevyengine/bevy/tree/v0.20.0)
