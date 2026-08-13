# Temporary Bevy 0.19 / wgpu 30 compatibility crates

This directory contains the published source of the Bevy 0.19.1 crates whose
public APIs use `wgpu-types`. Weld patches all of them together so the render
graph has one coherent wgpu 30 type generation. Patching only `bevy_render`
would leave structurally identical but incompatible wgpu 29 types crossing
crate boundaries.

The sources come from Bevy tag `v0.19.1`. The narrow compatibility changes are
derived from Bevy's upstream wgpu 30 migration, commit `5036d978a`. Exact
provenance and the included crate list are recorded in
`../bevy-wgpu30.upstream`.

Local changes are limited to dependency constraints, wgpu 30 API adaptations
in `bevy_render` and `bevy_pbr`, and the corresponding upstream WGSL changes.
Do not reformat or otherwise rewrite unrelated vendored source.

This is a temporary bridge, not a permanent Bevy fork. Remove this directory,
`../bevy-wgpu30.upstream`, and Weld's `[patch.crates-io]` entries when Weld
adopts a suitable Bevy release that natively depends on wgpu 30 or newer.
