# DRM rendering improvement plan

This is an active implementation plan for replacing Weld's production Vulkan
Display WSI presenter with a Smithay GBM/KMS output sink. It refines the DRM
ideas in [Possible future improvements](possible-future-improvements.md) into
testable slices. Only behavior recorded as implemented in
[Architecture](architecture.md) should be treated as current.

## Boundary

Physical presentation remains one consumer of Weld's composition, not the
owner of the application scene. `weld-app` owns a retained composition texture
and one stable Bevy manual-view handle. The backend may replace the concrete
view behind that handle for each render without retargeting cameras, UI,
picking, or plugins.

An active physical output uses this path:

```text
Smithay leases a GBM scanout DMA-BUF
    -> Vulkan foreign-queue acquire
    -> Bevy renders directly into that view
    -> scissored compositor-cursor pass
    -> Vulkan foreign-queue release
    -> KMS
```

When the VT or output is inactive, Bevy instead renders through the same handle
into the retained application texture. Client callbacks, ECS state, capture,
and future headless consumers therefore remain live without a scanout buffer.
A capture also selects the owned target for that frame, then requests a fresh
direct composition when physical presentation is available. DRM uses BGRA for
both targets so switching does not re-specialize Bevy pipelines.

## Implemented GBM/KMS and direct-target slices

Smithay owns the DRM device, connector/CRTC state, GBM swapchain, page flips,
and vblank retirement. Weld does not adopt Smithay render elements or a Smithay
renderer: [`GbmBufferedSurface`](https://smithay.github.io/smithay/smithay/backend/drm/struct.GbmBufferedSurface.html)
leases the next scanout DMA-BUF and accepts the completed buffer for KMS.
The selected buffer must have an explicit modifier present in the intersection
of KMS scanout and Vulkan sRGB color-attachment support. Weld fails during
presenter construction when Smithay's implicit-modifier compatibility fallback
is the only option, because the wgpu import path cannot safely infer its layout.

The calloop thread owns Smithay, KMS, the scanout import cache, and command
submission. A dedicated worker owns only the potentially blocking GPU
completion wait:

1. The host leases one buffer only while no physical frame is active.
2. The host imports and acquires that allocation as `Bgra8UnormSrgb`, then
   hands its view to the application host.
3. Bevy records and submits its complete output directly into the scanout
   image. Weld follows it with the cursor pass and foreign release on the same
   wgpu queue.
4. The worker waits for the final `SubmissionIndex` and wakes calloop with a
   prepared-frame event.
5. The host queues the completed buffer without a fence because the wait has
   already established completion.
6. Only a vblank from the buffer's CRTC retires the scanout frame and permits
   another direct composition.

The frame lifecycle is an explicit state machine. One active frame is either
rendering or awaiting vblank. New application demand remains coalesced in the
host `FrameState` while that target is busy. Tickets include presenter
generation, VT epoch, and frame identity so late worker or vblank events cannot
release newer ownership.

Direct rendering deliberately serializes Bevy composition behind scanout
availability. The former two-target path could render a newer offscreen frame
while the worker blitted the previous one; the direct path removes that
full-output bandwidth at the cost of waiting for GPU completion and vblank
before leasing the next target. Pending state remains coalesced, and a bounded
frame-interval wakeup prevents a lost presenter event from turning that
back-pressure into a permanent sleep or a zero-timeout spin.

### Vulkan ownership

Output buffers use an allocation-derived cache key: plane-zero DMA-BUF device
and inode plus size, FourCC, modifier, stride, and offset. Smithay may create a
new `Dmabuf` wrapper for every export, so wrapper identity is not a stable cache
key. The cache belongs to one GBM surface/swapchain generation. Clearing stale
presentation after a VT switch preserves its slots and cache; resetting the
swapchain, changing mode, resizing, or replacing the surface must drop the
matching import cache.

Each scanout image is tracked by wgpu as a color target while actual ownership
alternates with KMS. The shared wgpu queue orders these operations:

1. A raw-HAL-only submission acquires the image from foreign ownership and
   places it in `COLOR_ATTACHMENT_OPTIMAL`. First use begins from `UNDEFINED`.
2. Bevy submits its ordinary render graph, including the full-target output
   clear and store.
3. An ordinary cursor pass loads the initialized image and touches only its
   clamped scissor rectangle.
4. A raw-HAL-only submission returns the image to `GENERAL` and foreign
   ownership.

wgpu forbids mixing its ordinary and raw encoding APIs on one encoder. Separate
ordered submissions preserve that rule without a raw queue submission or a
wgpu source patch. The worker waits for the release submission's
`SubmissionIndex` before KMS receives the buffer. A native sync-file fence
should be investigated later so KMS can accept the commit before rendering
completes.

### Session recovery

On VT pause, Weld advances the presenter epoch and marks the physical sink
inactive before pausing the Smithay DRM device. Composition remains live. On
activation after an observed pause, Weld activates the device with a full state
reset, clears Smithay's stale pending/queued GBM leases, and requests a fresh
direct composition. The following queue operation performs the required
modeset. This avoids presenting stale owned-target contents or depending on a
page-flip event that may have been lost while another VT owned the display.

Transient allocation, import, commit, and vblank-retirement failures trigger a
bounded, event-driven scanout reset. A successfully retired vblank or explicit
session activation restores the retry budget. Exhaustion leaves only the
physical sink unavailable; a failed DRM event source is terminal for that
backend run because page flips can no longer be retired safely. Neither case
stops Bevy composition or disconnects clients.

## Deferred follow-ups

- Export a native completion fence instead of waiting on the presentation
  worker.
- Define simultaneous local-display and streaming consumers without restoring
  an unconditional full-output blit or rendering the scene twice by accident.
- Propagate output damage into KMS damage clips.
- Give each physical output its own state machine, composition target, camera,
  and vblank cadence.
- Add VRR, overlay promotion, and direct scanout as independently
  capability-gated optimizations. Atomic hardware cursor planes are implemented.
- Recreate output surfaces and their import caches for live mode changes rather
  than requiring restart.

## Acceptance

Build and policy checks cannot validate external-image ownership. The path is
accepted only after a real-TTY run proves:

- cold GBM/KMS startup and clean shutdown;
- foot and Firefox rendering and input;
- continued demand-driven client/headless composition into the owned target
  while another VT is
  active;
- return to the Weld VT with a fresh direct composition presented;
- repeated VT cycles without a stuck pending frame;
- validation-layer output proving `VK_LAYER_KHRONOS_validation` and
  synchronization validation actually loaded and reported no image-layout,
  ownership, lifetime, or synchronization error;
- explicit scanout format/modifier discovery on each supported AMD, Intel, and
  NVIDIA driver family before claiming compatibility with it.

Use `scripts/run-gbm-kms-validation` for validation-layer runs. The existing
Display WSI probe remains an independent driver diagnostic and historical
comparison until the GBM/KMS path is fully validated across supported driver
families.
