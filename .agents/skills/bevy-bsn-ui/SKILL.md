---
name: bevy-bsn-ui
description: Author or migrate Weld's Bevy 0.20 BSN scenes and UI composition. Use for scene functions, hierarchy, patches, lifecycle observers, headless widgets, clipping, and project-owned visual controls.
---

# Compose Weld UI with BSN

Read `CONTRIBUTING.md` and [bevy-020](../bevy-020/SKILL.md) first. Check the
resolved Bevy version: the syntax below targets 0.20, while the repository
remains on 0.19.1 until the dependency migration. Use the matching tagged
examples for pre-migration maintenance.

## Workflow

1. Express one conceptual entity as a small function returning `impl Scene`.
2. Express sibling roots as `impl SceneList` with `bsn_list!`.
3. Compose hierarchy inline with `Children [...]`, separating sibling entities
   with `--`. Use braces for the outer `bsn! {}` and `bsn_list! {}` invocation.
4. Parameterize ordinary scene functions first. Derive `SceneComponent` only
   for a project-owned conceptual control or a reusable scene with named props.
5. Attach UI behavior declaratively with `on(...)` observers and translate it
   into project-owned actions or state changes.
6. Use the existing host and scene-aware spawn path. When initialization needs
   the completed child hierarchy, use `on(... On<Ready> ...)` rather than a
   component-add observer. Layout measurements still require the layout pass.
7. Run formatting and the narrowest compile/test that exercises the scene.

## Syntax and composition

- Omit fields that should retain defaults. BSN entries are ordered patches, not
  full struct replacement.
- Compose scenes with `@scene`, `@scene_function()`, or `@{scene_expression}`.
  Pass component/template values directly; remove old `template_value(...)`
  wrappers. Braces still delimit complex Rust field expressions.
- Use `#Name` only for scoped names and references inside one macro invocation.
- Use `@Widget` for a `SceneComponent` and `@prop` for its immediate,
  non-patchable props.
- Apply call-site patches after a composed scene to customize layout,
  appearance, markers, or observers without copying its implementation.
- Use `Children [{children}]` for an `impl SceneList` slot in a reusable
  container.
- Keep conditionals in Rust before the macro where possible. Use expression
  interpolation when a conditional value belongs inside a scene.
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
- For enum entries, supply every field of the selected variant. The old
  `VariantDefaults` partial-variant pattern has been removed.
- Keep `.bsn` file loading explicit and tested. The 0.20 scene docs still label
  the built-in asset format as planned, despite asset-path examples.

Minimal 0.20 composition shape:

```rust
fn panel(contents: impl SceneList) -> impl Scene {
    bsn! {
        Node
        Children [
            Text("Weld")
            --
            {contents}
        ]
    }
}

fn screen() -> impl Scene {
    bsn! { @panel(bsn_list! { Text("Ready") -- Text("Connected") }) }
}
```

## Preserve a custom-UI path

Use headless widget behavior with Weld-owned visuals. Feathers provides
reference implementations when evaluating future controls:

```text
headless behavior + semantic/accessibility state + Weld visual scene
```

Keep application and compositor state authoritative outside widgets. Handle
headless events such as `Activate` and `ValueChange<T>` at a UI boundary, update
project-owned state, and reflect that state back into widget components. Domain
systems must not query presentation-specific marker types.

Use `bevy_ui_widgets::Button`, `Hovered`, and `Pressed` for new controls. For
tabs, inspect the controlled `TabList` example: translate
`ValueChange<Option<Entity>>` into a Weld action and publish authoritative
selection through `SelectedTab`. Keep widget focus separate from compositor
window focus. Use `tablist_self_update` only for locally owned UI selection.

When a repeated control warrants a Weld component, compose the unstyled
headless behavior with focus/navigation, semantics, accessibility, interaction
state, and a Weld-owned visual scene. Let concrete UI workflows establish the
requirements of shared controls.

## Geometry and text

- Treat `BorderRadius` corners as `CornerRadius` and resolved corners as 2D
  radii. Preserve circular intent with the circular constructors.
- Preserve transformed `CalculatedClip` geometry when mapping surface clips;
  an axis-aligned bounding box alone loses rotated clip edges.
- Keep client textures inside the existing rounded content container. Apply
  pixel snapping to the surface boundary while retaining smooth shell motion.
- Use `FixedNode` for deliberately viewport-relative overlays. Its root-like
  layout, transform and clipping behavior makes it inappropriate for content
  that must remain clipped by a window frame.
- Retain explicit configured font sizes where required. Default text now uses
  `RemSize`; `em`/`rem` are useful for controls whose spacing follows typography.
- Render-world paint hooks must account for retained nodes and invalidation;
  read the migration audit before changing the border-underlap hook.

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
- Verify syntax against the tagged 0.20 examples. Some API prose retains older
  snippets; cross-check conflicting examples against source before copying.

## Primary references

- [Bevy Scene 0.20 API and syntax](https://docs.rs/bevy_scene/0.20.0/bevy_scene/)
- [BSN example](https://github.com/bevyengine/bevy/blob/v0.20.0/examples/scene/bsn.rs)
- [Headless widget example](https://github.com/bevyengine/bevy/blob/v0.20.0/examples/ui/widgets/standard_widgets.rs)
- [Controlled and self-updating tabs](https://github.com/bevyengine/bevy/blob/v0.20.0/examples/ui/widgets/headless_tabs.rs)
- [UI node geometry source](https://github.com/bevyengine/bevy/blob/v0.20.0/crates/bevy_ui/src/ui_node.rs)
- [Migration audit](../bevy-020/references/migration-020.md)
