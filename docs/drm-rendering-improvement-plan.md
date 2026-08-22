# DRM output adapter plan

## Status

The Smithay-first startup multi-output adapter is implemented and awaiting
real-TTY multi-output validation. It replaced the removed custom GBM/KMS
presenter with Smithay's `DrmOutputManager` and a narrow wgpu renderer. See
[Direct DRM presentation](drm-presentation.md) for the exact current contract.

Implemented across the current slices:

- every usable startup desktop connector on the primary DRM GPU;
- Smithay-owned mode, swapchain, planes, commits, page flips, pause, and
  activation;
- direct Bevy rendering into an explicit-modifier Smithay lease;
- owned composition during inactive sessions and one-shot capture;
- per-output frame admission with deferred demand across matching vblanks;
- Smithay cursor-plane selection with a shared wgpu fallback; and
- refresh-derived independent pacing, aligned-output Bevy batching, and
  complete logical runtime scale updates.

The initial blocking GPU completion wait is a correctness baseline, not the
desired steady-state synchronization mechanism.

## Implemented multi-output architecture

Smithay leases one output target inside each `DrmOutput::render_frame` call,
while `AppShell::render_outputs` activates a selected set of output cameras and
runs one RenderApp pass. Weld's renderer batch records the external targets
during Smithay preparation, invokes Bevy once after all due targets are known,
then submits cursor fallback and foreign-release commands before queueing each
output independently.

The current contract:

1. Discovers an atomic startup layout; dynamic connector transactions remain
   deferred.
2. Acquires all due Smithay leases before one Bevy RenderApp pass.
3. Keeps one frame-admission state per output and retires it only from the
   matching CRTC vblank.
4. Preserves output entities, mixed-scale camera targets, window
   intersections, and logical pointer portals.
5. Quarantines an individual output render or queue failure while treating
   session inactivity as device-wide.

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
