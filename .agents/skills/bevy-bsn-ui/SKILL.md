---
name: bevy-bsn-ui
description: Author or refactor Weld's Bevy 0.19 scenes and UI composition with `bsn!`, `bsn_list!`, scene functions, patches, relationships, observers, and project-owned scene components. Use when evaluating or implementing UI hierarchies and reusable composition without assuming Feathers or Bevy's full application runtime.
---

# Compose Weld UI with BSN

Read `CONTRIBUTING.md` and the `bevy-019` skill first. Confirm that the current
Weld slice has deliberately adopted the necessary Bevy scene/UI crates before
writing BSN code.

## Workflow

1. Express one conceptual entity as a small function returning `impl Scene`.
2. Express sibling roots as `impl SceneList` with `bsn_list!`.
3. Compose hierarchy inline with `Children [...]`; parenthesize entities with
   multiple entries when boundaries would otherwise be unclear.
4. Parameterize ordinary scene functions first. Derive `SceneComponent` only
   for a project-owned conceptual control or a reusable scene with named props.
5. Attach UI behavior declaratively with `on(...)` observers and translate it
   into project-owned actions or state changes.
6. Keep the host and spawn path explicit; do not add Bevy `App`, Winit, or
   `DefaultPlugins` solely to match an example.
7. Run formatting and the narrowest compile/test that exercises the scene.

## Syntax and composition

- Omit fields that should retain defaults. BSN entries are ordered patches, not
  full struct replacement.
- Use `{expression}` for arbitrary Rust or a dynamic scene/list.
- Use `#Name` only for scoped names and references inside one macro invocation.
- Use `@Widget` for a `SceneComponent` and `@prop` for its immediate,
  non-patchable props.
- Apply call-site patches after a composed scene to customize layout,
  appearance, markers, or observers without copying its implementation.
- Use `Children [{children}]` for an `impl SceneList` slot in a reusable
  container.
- Keep conditionals in Rust before the macro where possible. BSN 0.19 has no
  native `if` or `match` syntax.
- Build runtime-length homogeneous children as `Vec<S>` where `S: Scene`, or
  heterogeneous children as `Vec<Box<dyn SceneList>>`, then interpolate the
  collection.
- Remember that `Scene` and `SceneList` are `Send + Sync + 'static`. Move owned
  labels, handles, and IDs into returned scenes instead of borrowing transient
  data.
- Keep each `#Name` reference inside the invocation that declares it. Composed
  scene functions and components create their own name scopes.
- Avoid scene caching until profiling demonstrates a need and its update
  semantics are understood.

## Preserve a custom-UI path

Do not add Feathers by default. Use its implementation architecture only as a
reference for future Weld controls:

```text
headless behavior + semantic/accessibility state + Weld visual scene
```

Keep application and compositor state authoritative outside widgets. Handle
headless events such as `Activate` and `ValueChange<T>` at a UI boundary, update
project-owned state, and reflect that state back into widget components. Domain
systems must not query presentation-specific marker types.

When a repeated control warrants a Weld component, compose the unstyled
headless behavior with focus/navigation, semantics, accessibility, interaction
state, and a Weld-owned visual scene. Do not build that design system before a
concrete UI workflow establishes its requirements.

## Ownership and review

- Keep a scene, its markers, observers, and state synchronization with the
  feature that owns the workflow.
- Keep top-level shells focused on composition and major placement.
- Extract shared visual infrastructure only after at least two features need
  it; do not create empty UI modules or crates.
- Give every interactive control an accessible label and keyboard/focus
  behavior appropriate to its role.
- Verify emitted events update authoritative state rather than relying on
  incidental visual state.
- Verify scene functions do not capture references that violate the returned
  scene's lifetime.
- Verify syntax and component names against Bevy 0.19, not `main`.

## Primary references

- [Bevy 0.19 BSN overview](https://bevy.org/news/bevy-0-19/#next-generation-scenes)
- [Bevy Scene 0.19 API and syntax](https://docs.rs/bevy_scene/0.19.0/bevy_scene/)
- [Bevy 0.19 BSN example](https://github.com/bevyengine/bevy/blob/v0.19.0/examples/scene/bsn.rs)
- [Bevy 0.19 headless widget API](https://docs.rs/bevy/0.19.0/bevy/ui_widgets/)
- [Bevy standard headless widget example](https://bevy.org/examples/ui-user-interface/standard-widgets/)
