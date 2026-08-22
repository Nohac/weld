# Direct DRM presentation

Weld's production DRM backend is intentionally absent at the clean-room
baseline. The previous implementation manually owned GBM swapchains, primary
buffer queueing, page-flip retirement, cursor-plane commits, recovery state,
and device-wide frame scheduling on top of Smithay's low-level
`GbmBufferedSurface`. It was removed so the replacement cannot accidentally
retain that parallel output stack.

`HostBackend::Drm` remains a stable selection boundary. Selecting it currently
returns an explicit error rather than falling back to nested mode. Automatic
backend selection therefore still reveals a missing standalone host on a bare
TTY.

## Validated boundary

The focused `smithay_drm_compositor_probe` proves the intended replacement
seam with the pinned Smithay tree:

```text
DrmOutputManager and DrmCompositor own output state and lease a primary DMA-BUF
    -> Weld imports that allocation into Vulkan and wgpu
    -> wgpu renders directly into the leased image
    -> Weld returns foreign ownership after GPU completion
    -> Smithay queues and retires the frame through KMS
```

The probe validates cold startup, explicit-modifier import, page-flip
retirement, VT pause and activation, and clean shutdown on the tested AMD/RADV
system. It performs no CPU pixel copy or output-sized GPU blit. See
[Smithay integration validation](smithay-integration-validation.md) for the
recorded evidence and ownership table.

Run the reference from a real TTY:

```text
scripts/run-smithay-drm-compositor-probe
```

The older `drm_wsi_probe` is retained only as a driver diagnostic and historical
comparison for Vulkan Display WSI:

```text
scripts/run-drm-wsi-probe --seconds 30
```

Neither example is a production compositor backend.

## Replacement constraints

The production adapter must:

- delegate connector, CRTC, mode, swapchain, KMS plane, damage, page-flip, and
  activation lifecycle to Smithay's output compositor;
- keep Weld's Wayland surface model, Bevy scene, window entities, and plugin
  policy outside Smithay's desktop model;
- implement only the narrow renderer traits needed to bind a Smithay-leased
  DMA-BUF as Weld's wgpu composition target;
- preserve an owned target for capture, headless operation, streaming, and
  continued application updates while the physical session is inactive;
- use page-flip metadata for physical pacing and tolerate missing DRM
  timestamps with a local monotonic fallback;
- treat hardware cursor, direct scanout, overlay promotion, VRR, and native
  completion fences as capabilities layered on the same output lifecycle;
- recover from output loss, suspend, and failed commits without disconnecting
  clients or taking down non-physical consumers; and
- avoid restoring the removed low-level presenter as a compatibility fallback.

The first production slice may retain the probe's blocking GPU completion wait.
Exporting a native completion fence is a later optimization, but no CPU pixel
copy or full-output blit is acceptable in the new fast path.
