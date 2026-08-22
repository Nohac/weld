# DRM output adapter plan

## Status

The first single-output adapter is implemented and awaiting real-TTY
validation. It replaced the removed custom GBM/KMS presenter with Smithay's
`DrmOutputManager` and a narrow wgpu renderer. See
[Direct DRM presentation](drm-presentation.md) for the exact current contract.

Implemented in this slice:

- one preferred desktop connector on the primary DRM GPU;
- Smithay-owned mode, swapchain, planes, commits, page flips, pause, and
  activation;
- direct Bevy rendering into an explicit-modifier Smithay lease;
- owned composition during inactive sessions and one-shot capture;
- one-frame admission with deferred demand across vblank;
- Smithay cursor-plane selection with a shared wgpu fallback; and
- refresh-derived pacing and logical runtime scale updates.

The initial blocking GPU completion wait is a correctness baseline, not the
desired steady-state synchronization mechanism.

## Next architecture slice: multiple outputs

Smithay leases one output target inside each `DrmOutput::render_frame` call,
while `AppShell::render_outputs` currently activates a set of output cameras
and runs one RenderApp pass. The next design must reconcile those lifetimes
without retaining Smithay frames across unrelated callbacks, rendering the
same camera twice, or making one output's failure block the rest.

That slice should:

1. Represent enabled connector changes as an atomic Weld output-layout
   transaction.
2. Establish how all required Smithay leases are acquired before the one Bevy
   RenderApp pass, or deliberately prove that independent per-output passes
   preserve extraction and client-buffer ownership.
3. Keep one frame-admission state per output and retire it only from the
   matching CRTC vblank.
4. Preserve the existing output entities, mixed-scale camera targets, window
   intersections, and physical pointer topology.
5. Handle connector removal without disconnecting clients or destroying owned
   headless consumers.

The historical mixed-scale observations remain in
[Multi-output validation](multi-output-validation.md).

## Subsequent capabilities

- Export a native completion fence instead of waiting for wgpu.
- Publish real element damage as Bevy retained rendering matures.
- Expose eligible unadorned client buffers for primary direct scanout or
  overlay promotion without bypassing Weld policy.
- Add dynamic connector and mode changes.
- Add `wp_presentation`, VRR policy, HDR, color management, rotation, and
  cross-GPU transfer.

## Acceptance status

Automated workspace checks cover the protocol-neutral policies and compile the
complete backend. Hardware acceptance remains pending and requires the real-TTY
matrix recorded in [Direct DRM presentation](drm-presentation.md). Do not claim
AMD, Intel, or NVIDIA support until each driver family has explicit evidence.
