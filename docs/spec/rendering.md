# Rendering

## Current composition — Implemented

Bevy renders client surfaces and compositor UI into a core-owned composition
target, and a project-owned wgpu pass presents or captures it. Client DMA-BUFs
are imported as external Vulkan images and sampled with no CPU pixel copy, GPU
normalization blit, or intermediate surface texture; SHM pixels are copied into
Bevy images. Detailed ownership and synchronization live in
[Architecture](../architecture.md).

Standalone DRM uses Smithay's GBM/KMS surface as its physical output sink, but
keeps composition in wgpu and does not use a Smithay renderer. The first path
performs one full-screen GPU blit from Bevy's offscreen composition into each
leased scanout buffer. Removing that blit while preserving headless and capture
composition is the next pass in the
[DRM rendering improvement plan](../drm-rendering-improvement-plan.md). Nested
and DRM operation, VT recovery, output loss, and the retained historical WSI
probe are documented in [Direct DRM presentation](../drm-presentation.md).

## Frame demand — Implemented

Rendering is event driven. Surface and host changes request composition;
continuous Bevy visuals emit `RequestRedraw` while active; unchanged client
buffers are retained and reused. Output refresh is an upper presentation
opportunity, not a requirement to redraw every visible client every refresh.
Physical output availability does not gate demand-driven composition, so
clients and non-display consumers can keep progressing while an output is
inactive.

## Composition model — Direction

Client content must remain an ordinary scene element that can participate in
Bevy transforms, nested clipping, opacity, rounded corners, borders, shadows,
text, images, hit testing, and compositor-owned UI. Plugins work with Weld and
Bevy resources, never native image handles or synchronization primitives.

Each physical output should ultimately have its own composition target and
camera rather than one giant cross-monitor texture. Output-independent policy
keeps logical entities stable; presentation retargets their roots when they
move between outputs.

## Performance and display work — Direction

- Propagate client, surface-tree, shell, animation, and output damage so work
  is bounded to changed content where Bevy permits it.
- Keep frame callbacks, encoding, input, and backend events independent from a
  blocked presenter.
- Allow fullscreen surfaces to become eligible for direct scanout or hardware
  planes without making that path a prerequisite for correctness.
- Add VRR, presentation timing, HDR, color management, and multi-GPU behavior
  as explicit capability-driven paths.

The repository's existing measurements are diagnostic observations, not
performance promises. Reproduction notes live under the deferred baseline in
[Direct DRM presentation](../drm-presentation.md#deferred-performance-baseline).

## Open work — Exploration

- Whether a future retained Bevy UI path removes enough unchanged UI work to
  replace project-specific invalidation.
- More direct SHM-to-GPU upload strategies that avoid an extra staging copy
  without weakening buffer lifetime rules.
- Cross-GPU import, transfer, and per-output renderer ownership.
