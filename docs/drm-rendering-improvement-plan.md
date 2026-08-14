# DRM rendering improvement plan

This is an active implementation plan for replacing Weld's production Vulkan
Display WSI presenter with a Smithay GBM/KMS output sink. It refines the DRM
ideas in [Possible future improvements](possible-future-improvements.md) into
testable slices. Only behavior recorded as implemented in
[Architecture](architecture.md) should be treated as current.

## Boundary

Physical presentation remains one consumer of Weld's composition, not the
owner of the application scene. Bevy continues rendering into an offscreen
composition target while clients, capture, headless operation, or future
streaming require it. Losing the active VT or every connector suspends only the
physical output sink; it does not tear down clients, ECS state, or the renderer.

The first GBM/KMS slice deliberately retains this path:

```text
Bevy offscreen composition
    -> full-screen wgpu blit plus compositor cursor
    -> Smithay/GBM scanout DMA-BUF
    -> KMS
```

That final blit is GPU-only and preserves the current headless and capture
lifecycle, but it still reads and writes every output pixel. The next rendering
pass should investigate binding a leased GBM buffer directly to the output
camera when physical presentation is the only consumer, falling back to an
offscreen target when the output is inactive or another consumer needs an
independent frame. Removing the blit is not part of the first slice.

## First slice: GBM/KMS sink

Smithay owns the DRM device, connector/CRTC state, GBM swapchain, page flips,
and vblank retirement. Weld does not adopt Smithay render elements or a Smithay
renderer: [`GbmBufferedSurface`](https://smithay.github.io/smithay/smithay/backend/drm/struct.GbmBufferedSurface.html)
leases the next scanout DMA-BUF and accepts the completed buffer for KMS.
The selected buffer must have an explicit modifier present in the intersection
of KMS scanout and Vulkan sRGB color-attachment support. Weld fails during
presenter construction when Smithay's implicit-modifier compatibility fallback
is the only option, because the wgpu import path cannot safely infer its layout.

The calloop thread owns Smithay and KMS objects. A dedicated worker owns the
potentially blocking GPU completion wait:

1. The host leases one buffer only while no physical frame is active.
2. The worker imports that allocation as `Bgra8UnormSrgb`, composites the
   offscreen frame and cursor into it, and waits for the wgpu submission.
3. The worker wakes calloop with a prepared-frame event.
4. The host queues the completed buffer without a fence because the wait has
   already established completion.
5. Only a vblank from the buffer's CRTC retires the scanout frame and permits
   the newest coalesced frame to proceed.

The frame lifecycle is an explicit state machine. One active frame is either
rendering or awaiting vblank, and one pending slot retains only the newest
complete composition. Tickets include presenter generation, VT epoch, frame
identity, and composition-target identity so late worker or vblank events
cannot release newer ownership.

### Vulkan ownership

Output buffers use an allocation-derived cache key: plane-zero DMA-BUF device
and inode plus size, FourCC, modifier, stride, and offset. Smithay may create a
new `Dmabuf` wrapper for every export, so wrapper identity is not a stable cache
key. The cache belongs to one GBM surface/swapchain generation. Clearing stale
presentation after a VT switch preserves its slots and cache; resetting the
swapchain, changing mode, resizing, or replacing the surface must drop the
matching import cache.

Each scanout image is tracked by wgpu as a color target while actual ownership
alternates with KMS. One wgpu submission contains three ordered command
buffers:

1. A raw-HAL-only encoder acquires the image from foreign ownership and places
   it in `COLOR_ATTACHMENT_OPTIMAL`. First use begins from `UNDEFINED`.
2. An ordinary-wgpu-only encoder performs the blit.
3. A raw-HAL-only encoder returns the image to `GENERAL` and foreign ownership.

wgpu forbids mixing its ordinary and raw encoding APIs on one encoder. The
three-encoder batch preserves that rule without a raw queue submission or a
wgpu source patch. The worker waits for the batch's one `SubmissionIndex`
before KMS receives the buffer. A native sync-file fence should be investigated
later so KMS can accept the commit before rendering completes.

### Session recovery

On VT pause, Weld advances the presenter epoch and marks the physical sink
inactive before pausing the Smithay DRM device. Composition remains live. On
activation after an observed pause, Weld activates the device with a full state
reset, clears Smithay's stale pending/queued GBM leases, and requests the newest
completed composition. The following queue operation performs the required
modeset. This avoids depending on a page-flip event that may have been lost
while another VT owned the display.

Transient allocation, import, commit, and vblank-retirement failures trigger a
bounded, event-driven scanout reset. A successfully retired vblank or explicit
session activation restores the retry budget. Exhaustion leaves only the
physical sink unavailable; a failed DRM event source is terminal for that
backend run because page flips can no longer be retired safely. Neither case
stops Bevy composition or disconnects clients.

## Deferred follow-ups

- Remove the full-screen blit when a directly leased output target is safe for
  every active consumer.
- Export a native completion fence instead of waiting on the presentation
  worker.
- Propagate output damage into KMS damage clips.
- Give each physical output its own state machine, composition target, camera,
  and vblank cadence.
- Add VRR, hardware cursor planes, overlay promotion, and direct scanout as
  independently capability-gated optimizations.
- Recreate output surfaces and their import caches for live mode changes rather
  than requiring restart.

## Acceptance

Build and policy checks cannot validate external-image ownership. The path is
accepted only after a real-TTY run proves:

- cold GBM/KMS startup and clean shutdown;
- foot and Firefox rendering and input;
- continued demand-driven client/headless composition while another VT is
  active;
- return to the Weld VT with the newest composition presented;
- repeated VT cycles without a stuck pending frame;
- validation-layer output proving `VK_LAYER_KHRONOS_validation` actually loaded
  and reported no image-layout, ownership, lifetime, or synchronization error.
- explicit scanout format/modifier discovery on each supported AMD, Intel, and
  NVIDIA driver family before claiming compatibility with it.

Use `scripts/run-gbm-kms-validation` for the validation-layer run once the first
slice is implemented. The existing Display WSI probe remains an independent
driver diagnostic and historical comparison until the GBM/KMS path is fully
validated.
