# Weld's Bevy 0.20 migration audit

Migrated to `v0.20.0` on 2026-10-22. Recheck manifests and patch history when
refreshing dependencies.

## Vendor removal gates

The [0.20 render manifest](https://github.com/bevyengine/bevy/blob/v0.20.0/crates/bevy_render/Cargo.toml)
uses wgpu 30, matching Weld's graphics boundary. The twelve-crate compatibility
tree was removed. Two narrow patches remain under `vendor/bevy-render` and
`vendor/bevy-ui`:

| Local behavior | 0.20 upstream status | Weld validation gate |
| --- | --- | --- |
| Manual texture-view scale | Camera target info still reports `1.0` | Preserve fractional scaling and per-output logical geometry. |
| Manual-target metric change detection | Manual targets still count as changed every update | Texture rotation must preserve stable projection/UI metrics while size and scale changes invalidate them. |
| Tracy timestamp capability guard | GPU-context construction still assumes timestamp features | Profiling on Weld-created devices must respect their enabled capabilities. |
| Fractional border insets | Length-valued borders are floored even with layout rounding disabled | A 3px logical border at 1.25 scale must retain its 3.75px physical inset. |

Sources: [camera](https://github.com/bevyengine/bevy/blob/v0.20.0/crates/bevy_render/src/camera.rs),
[Tracy](https://github.com/bevyengine/bevy/blob/v0.20.0/crates/bevy_render/src/diagnostic/tracy_gpu.rs).
Read `vendor/bevy-render.upstream` and `vendor/bevy-ui.upstream` for provenance
and removal criteria. Prefer supported integration points for equivalent
behavior; preserve regression coverage before dropping a patch.

Check both root `Cargo.toml` and `apps/weld-mobile/Cargo.toml` before removing
shared Bevy paths. Resolve Bevy-dependent Leafwing versions with the desktop
graph. SVG artwork uses the project-owned resvg adapter `weld-svg`. Smithay,
VA-API and AnyRender patches have independent
provenance and removal criteria in their vendor records.

## Render integration

Read the [retained UI implementation](https://github.com/bevyengine/bevy/blob/v0.20.0/crates/bevy_ui_render/src/lib.rs)
when adapting `crates/weld-app/src/border.rs`:

- Extraction keeps unchanged nodes. `ExtractedUiNodes` groups items by main
  entity and camera; its `changed` set identifies re-extracted entities.
- Apply border underlap to freshly extracted geometry, or derive it
  idempotently. Repeatedly adding overlap to retained geometry would accumulate.
- Account for changes to Weld-owned markers and referenced backing nodes,
  including removal: upstream invalidation knows only its own dependencies.
- Keep hide/show, camera reassignment and despawn cleanup covered. Transparent
  UI still has an incomplete retention migration; inspect custom materials
  separately before estimating savings.
- Bevy clears ordinary `UiBatch` components during render cleanup. Weld replaces
  that registration after plugin initialization with conditional cleanup, using
  the public schedule API. Retained replay must keep those batches as well as
  extraction data; otherwise ordinary images disappear after the first frame.
- Preserve the exact circular clip distance in Weld's surface shader. The
  upstream elliptical approximation with derivative-based antialiasing softened
  interior texels in the fractional-scale regression. Elliptical corners use
  the upstream distance function.

For `surface.rs`, `surface/clip.rs`, `surface/snap.rs`, and the surface shader,
check transformed clipping, 2D corner radii and logical/physical coordinates.
Keep the native import, immutable parameter publication and bind-group cache
contracts intact. A newly published buffer must draw with the same parameters
used to form its cache key, including when no policy update follows.

The [migration guide](https://bevy.org/learn/migration-guides/0-19-to-0-20/)
identifies these additional boundaries to inspect:

- Bevy shaders use WESL; translate preprocessor imports/defines and rename
  affected assets. Plain WGSL remains usable. Inspect `surface_material.wesl`
  and its embedded asset registration together.
- Extraction traits identify the target app, such as
  `ExtractComponent<RenderApp>` / `#[extract_app(RenderApp)]`; implementation
  moved into `bevy_extract` with render re-exports.
- Window render data moved from resource maps to components/queries.
- `UnpreparedBindGroup` became `BindGroupBuilder`; compositing-space state and
  camera-stack indexing changed. Verify custom pipeline and color handling.
- Lifecycle observers use `On<Add<T>>`; pointer events use flat types such as
  `PointerPress`. Audit event field access as well as type names.
- Font loading/source APIs changed. Preserve system-font resolution and the
  explicit sizes derived from Sway configuration.

Use the [tagged shader example](https://github.com/bevyengine/bevy/blob/v0.20.0/examples/shader/shader_material_2d.rs)
as an asset/material wiring reference. Follow the target-version UI material
source for Weld's actual UI pipeline.

## Validation sequence

1. Keep dependency edits narrow and preserve the shared desktop feature graph.
   Read `CONTRIBUTING.md` for package selection, environment and build commands.
2. Check ECS policy and BSN compilation before widening to the renderer tests.
3. Run the offscreen `weld-app` shell regressions for scaling, border underlap,
   clipping, late binding publication, retained frames and resource cleanup.
4. Exercise repeated workspace hide/show, tabs, output resize/scale, screenshot
   capture and session locking through the relevant host probes/manual checks.
5. Compare equivalent optimized profiling runs only when the user requests
   performance investigation. Record static chrome, 60 Hz content-only updates,
   and focus/tab churn separately. Confirm present timing as well as CPU cost.
6. Validate affected standalone clients before deleting their vendor paths.
   Installing into the user's login session remains a separate requested step.

The [release notes](https://bevy.org/news/bevy-0-20/) also introduce summary
change ticks and schedule randomization. Evaluate these as follow-up tools
after the compatibility migration, keeping measured improvements distinct
from dependency-upgrade correctness.
