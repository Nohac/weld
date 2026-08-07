---
name: smithay
description: Implement, review, debug, or update Weld's Smithay compositor host against the pinned vendored upstream tree. Use for Wayland protocol dispatch, calloop integration, nested or DRM backends, seats and input, surfaces, outputs, rendering, DMA-BUF, XWayland, Smithay feature selection, upstream source lookup, local patches, or vendor updates.
---

# Work with Smithay in Weld

1. Read `CONTRIBUTING.md` before changing code or dependencies.
2. Treat `vendor/smithay` as the API and implementation source of truth once it
   exists. Read the recorded revision in `vendor/smithay.upstream`.
3. Read upstream `vendor/smithay/AI.md` before preparing a contribution for
   Smithay.
4. Search the vendored source and matching examples before relying on memory,
   released docs, or current online master docs.
5. Preserve the Smithay/calloop host boundary and keep compositor policy
   independently testable.
6. Run focused checks in debug mode and exercise the affected backend where
   practical.

## Find the matching implementation

Search in this order:

1. `vendor/smithay/smallvil` for the smallest complete compositor pattern.
2. `vendor/smithay/examples` for isolated backend or protocol examples.
3. `vendor/smithay/anvil` for production-shaped backend, rendering, input, and
   protocol integration.
4. `vendor/smithay/src` for the actual trait, delegate macro, invariant, and
   implementation.

Use `rg` to find the trait and all call sites together. Smithay APIs commonly
require a state trait implementation plus a delegate macro; copy the complete
pattern from the pinned revision rather than mixing snippets from different
versions.

Generate local API documentation when source navigation is insufficient:

```text
CARGO_TARGET_DIR=target/smithay-docs cargo doc \
  --manifest-path vendor/smithay/Cargo.toml \
  -p smithay --no-deps --no-default-features --features <features>
```

Online master documentation is useful for discovery only. Confirm every API
against the vendored revision before using it.

## Preserve Weld's boundaries

- Let `calloop` own the outer process loop and Smithay protocol/backend
  lifetimes. Drive Bevy's `App` explicitly from that host loop.
- Keep raw Wayland resources, Smithay handles, backend objects, renderer
  objects, and callback-bound borrows out of general ECS policy.
- Translate host events into owned Weld data before running policy. Translate
  policy results into typed host effects and validate them before applying
  Smithay operations.
- Keep Smithay's renderer abstractions distinct from Bevy's wgpu renderer.
  Prove buffer import, synchronization, render-target ownership, and
  presentation interop before sharing GPU resources or designing a common
  abstraction.
- Keep nested-backend shortcuts explicit. Do not let assumptions from Winit or
  X11 development backends leak into DRM/session behavior.
- Use stable Weld identifiers outside the host boundary; do not expose
  Smithay objects or ECS entities through configuration, IPC, or plugins.

## Select features deliberately

Disable Smithay default features and enable only those required by the current
host. For a nested experiment, inspect the pinned feature graph before choosing
between `backend_winit` and `backend_x11`; do not enable DRM, libinput, session,
XWayland, or every renderer preemptively.

Remember that Smithay's `backend_x11` feature transitively enables its DRM and
GBM backends and their native libraries. Use `backend_winit` when the nested
host should remain independent of that stack.

When a feature changes, inspect both Smithay's `Cargo.toml` and the native
libraries it activates. Keep the project environment and contributor commands
in sync with the selected backend.

## Maintain the vendored tree

- Import the complete upstream repository with squashed history so `smallvil`,
  `anvil`, examples, source, licenses, and contributor policy remain available.
- Pin every import to an exact upstream commit. Never build from a moving
  `master` reference.
- Use a path dependency on `vendor/smithay`; do not keep a second Git or
  crates.io Smithay dependency in the graph.
- Record the upstream repository, branch, and exact commit in
  `vendor/smithay.upstream`, outside the subtree.
- Preserve upstream license files and notices.
- Keep Weld-specific Smithay patches as isolated commits that touch the subtree
  only. Do not mix them with compositor feature work.
- Do not reformat, lint-fix, or mechanically rewrite unrelated vendored code.
- Update through a reviewed squashed subtree merge. Inspect upstream changes,
  resolve local patch conflicts deliberately, update the revision record, and
  run all host/backend checks before accepting the update.
- Prefer upstreamable fixes. Before sending one upstream, separate it from
  Weld assumptions and follow Smithay's current contribution and AI policies.

## Review checklist

- Verify each delegate macro matches its state trait implementation.
- Verify client and protocol errors are isolated rather than crashing the
  compositor.
- Verify surface commits, frame callbacks, buffer release, and damage handling
  happen at the correct lifecycle points.
- Verify callback ownership does not require unnecessary `Rc`, `Arc`, or locks.
- Verify backend events cannot run Bevy policy reentrantly.
- Verify selected native features build in the documented development
  environment.
- Test nested startup, client connection, surface mapping, redraw, resize, and
  clean shutdown when those paths are affected.

## Primary references

- [Smithay repository](https://github.com/Smithay/smithay)
- [Smithay master documentation](https://smithay.github.io/smithay/)
- [Smithay documentation index](https://smithay.github.io/pages/documentation.html)
- [Smithay handbook](https://smithay.github.io/book/)
