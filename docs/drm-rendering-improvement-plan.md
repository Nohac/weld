# DRM output adapter plan

This plan starts at Weld's clean-room baseline. It replaces the removed custom
GBM/KMS presenter with a small adapter around the public Smithay boundary proven
by `smithay_drm_compositor_probe`.

## Ownership

Smithay owns:

- session, device, connector, CRTC, mode, and page-flip lifecycle;
- primary swapchains, framebuffer export, plane assignment, and damage state;
- device-wide output coordination and activation recovery; and
- hardware cursor and direct-scanout eligibility when Weld supplies suitable
  render elements.

Weld owns:

- Wayland protocol state and client-buffer lifetime;
- Bevy application updates, scene composition, and window policy;
- selection between physical, retained, capture, headless, and streaming
  targets; and
- the narrow Vulkan/wgpu import and synchronization implementation required by
  Smithay's renderer traits.

Smithay desktop helpers must not become a second source of truth beside Weld's
managed-window entities.

## Initial implementation slice

1. Extract the proven DMA-BUF binding and foreign-ownership code from the probe
   into a production renderer adapter without broadening its responsibility.
2. Construct one `DrmOutputManager` and `DrmOutput` per discovered physical
   output using Smithay's session and udev integration.
3. Bind the Smithay-leased primary image to the stable manual texture-view
   target used by the matching Bevy output camera.
4. Drive application composition only when that output has demand and Smithay
   can accept a frame, then submit through `DrmOutput::render_frame`.
5. Retire physical work from the matching page-flip event and feed presentation
   metadata back to the Wayland server.
6. On session pause, stop physical queueing and select the owned target. On
   activation, call Smithay's activation path and request a fresh full frame.
7. Keep failures local to the physical output whenever clients and retained
   composition can continue safely.

The adapter may initially wait for wgpu completion as the probe does. That wait
must stay outside protocol dispatch if it can block materially. Native fence
export should replace it without changing ownership.

## Follow-up capabilities

- expose a stable cursor render element so Smithay can use a hardware cursor
  plane, with GPU composition as an explicit capability fallback;
- publish real damage and element commit state as Bevy gains retained rendering;
- expose eligible unadorned client buffers for direct scanout or overlay
  promotion without bypassing Weld policy;
- coordinate multiple outputs and mixed scales without rendering two copies on
  the same camera;
- add live connector and mode changes as whole-layout transactions;
- select VRR policy independently per output; and
- keep rendering into owned targets when physical presentation is suspended or
  intentionally detached for streaming.

## Acceptance

Automated checks cover the protocol-neutral and nested boundaries. A physical
adapter is accepted only after real-TTY validation proves:

- cold startup and orderly shutdown;
- foot and Firefox rendering and input;
- explicit-modifier direct rendering with no CPU pixel copy or full-frame blit;
- repeated VT pause and activation with a fresh frame after return;
- continued retained or headless composition while the VT is inactive;
- connector removal and restoration without a compositor crash;
- Vulkan validation with no image-layout, lifetime, or synchronization errors;
  and
- explicit evidence on each driver family before claiming AMD, Intel, or
  NVIDIA support.

Until this adapter exists, `HostBackend::Drm` must fail explicitly and the
Smithay compositor probe remains the physical-output reference.
