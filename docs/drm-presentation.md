# Direct DRM presentation

Weld has a production-shaped single-output DRM backend built on Smithay's
`DrmOutputManager` and `DrmOutput`. The code compiles and its Weld-owned policy
is covered by automated tests. Real-TTY validation of this implementation is
still required before its behavior is recorded as hardware evidence.

The previous implementation manually owned GBM swapchains, primary buffer
queueing, page-flip retirement, cursor-plane commits, recovery state, and
device-wide frame scheduling on top of Smithay's low-level
`GbmBufferedSurface`. That path remains deleted. There is no compatibility
fallback to it and selecting DRM never silently selects the nested backend.

## Implemented ownership

The active path is:

```text
DrmOutputManager and DrmCompositor lease an explicit-modifier primary DMA-BUF
    -> Weld imports the allocation into the shared Vulkan and wgpu device
    -> Bevy renders directly into that leased image
    -> optional cursor fallback draws after Bevy
    -> Weld returns foreign ownership after GPU completion
    -> Smithay queues and retires the frame through KMS
```

Smithay owns the session-facing DRM device, CRTC, mode, GBM swapchain,
framebuffer export, plane assignment, cursor-plane eligibility, atomic commit,
page flip, pause, and activation lifecycle. Weld owns the Wayland server, Bevy
scene, frame demand, target selection, callback payload, and the unavoidable
wgpu Vulkan-import seam.

The first slice deliberately enables one desktop connector. It prefers an
internal `eDP`, `LVDS`, or `DSI` connector, rejects connectors carrying the DRM
`non-desktop` property, and logs any additional desktop connectors as deferred.
The selected preferred DRM mode supplies the physical extent and refresh
interval. Runtime scaling changes only logical output state and never resizes
the leased scanout buffers.

## Composition and synchronization

The output contains one stable, opaque Smithay render element representing the
complete Bevy scene. A changed scene increments that element's commit counter.
When Smithay draws it, Weld temporarily binds the leased view to the output's
stable Bevy manual target and runs the existing RenderApp. There is no CPU
pixel copy, normalization texture, or output-sized GPU blit.

The import cache follows Smithay swapchain-slot lifetime through `WeakDmabuf`.
Binding an unchanged slot does not acquire it. Foreign queue-family acquisition
happens only when Smithay creates a renderer frame. Bevy submits its work before
the foreign release barrier; the initial implementation waits for that release
submission and returns a signalled Smithay synchronization point. A native
completion fence should later replace the blocking wait without changing the
ownership contract.

At most one physical frame is admitted at once. A render request arriving while
that frame is queued is retained and re-armed after its vblank rather than being
cleared with the older frame. `EmptyFrame` never creates an in-flight record,
and `DeviceInactive` redirects subsequent work to the owned target.

The matching DRM vblank is the active physical output's pacing clock. Retiring
a frame clears the interval fallback deadline, so buffered Bevy input and dirty
composition work are admitted immediately after vblank with enough time to
reach the next refresh. Nested and inactive-owned targets retain interval-based
pacing because they have no physical retirement event.

## Cursor

Named and client cursor images are normalized into Smithay
`MemoryRenderBuffer`s. Cursor shape names come from `cursor-icon`; Xcursor theme
inheritance and parsing come from the `xcursor` crate. The ordinary normalized
form is premultiplied `Argb8888`, has a normal transform, an identity source,
an explicit logical size, and integer physical placement.

Weld supplies the cursor as `Kind::Cursor` and enables only
`ALLOW_CURSOR_PLANE_SCANOUT`. Smithay decides whether to copy it into the GBM
cursor buffer or GPU-compose it. Primary and overlay direct scanout remain
disabled. Cursor-only motion requests presentation without dirtying the Bevy
scene; motion arriving during an in-flight flip is deferred until that flip
retires. An oversized cursor, rotated output, unsupported plane, or ineligible
geometry uses the shared composition blitter as a correctness fallback.

## Inactive and capture targets

The host has two explicit routes: `ActivePhysical` and `InactiveOwned`. It stops
queueing physical work before requesting a VT switch, pauses the Smithay output
manager when libseat reports suspension, and continues demand-driven Bevy work
against the owned target. Inactive client frame callbacks complete after a
successful owned submission.

Activation calls `DrmOutputManager::activate(true)`, completes callbacks from a
queued frame whose vblank can no longer arrive, and requests a fresh full
physical frame. Repeated notifications are harmless.

Capture always reads an owned `COPY_SRC` texture. An active capture performs a
one-shot owned composition, writes the PNG through the shared readback helper,
then requests a new direct physical composition. It never reads a scanout
allocation or introduces an owned-to-scanout blit.

## Validation

Run the production backend from a real TTY:

```text
scripts/run-weld-drm
scripts/run-weld-drm --seconds 30 foot
WELD_DRM_VALIDATE=1 scripts/run-weld-drm foot
WELD_DRM_PACING_TRACE=1 scripts/run-weld-drm foot
```

Output defaults to `target/validation/weld-drm.log`. The validation run must
still prove cold startup, foot and Firefox input, named and client cursors,
cursor-plane motion without Bevy rendering, GPU cursor fallback, capture,
runtime scaling, repeated VT transitions, inactive owned composition, fresh
presentation after activation, clean shutdown, and absence of Vulkan layout or
synchronization errors.

Use `--seconds` while exercising session transitions. The watchdog starts only
after the debug binary has compiled and terminates Weld through its normal
signal-driven shutdown path, so a failed input resume cannot leave the
operator trapped indefinitely.

The pacing trace records Smithay's cursor-plane assignment and composition
presentation state alongside DRM vblank sequence deltas, composition phase,
and blocking GPU wait duration. It is intended to distinguish a cursor-only
atomic commit from primary-plane rendering and to expose missed refreshes. The
trace is opt-in because synchronous log output can perturb the cadence being
measured; compare it with a trace-disabled run.

The focused Smithay probe remains useful for isolating the lower boundary:

```text
scripts/run-smithay-drm-compositor-probe
```

The older Vulkan Display WSI probe remains only a driver diagnostic.

## Deferred

- Multiple startup outputs, simultaneous Smithay leases, and hotplug.
- Native completion-fence export.
- Partial Bevy scene damage.
- Primary direct scanout and overlay promotion.
- `wp_presentation`, VRR, HDR, color management, rotation, and cross-GPU paths.
- EDID make/model/serial extraction; the optional `libdisplay-info` dependency
  is not enabled in the current development shell.
