# Smithay integration validation

This note records what Weld has verified in its pinned Smithay revision and the
small experiment used to choose the physical-output boundary. It is evidence
for a later refactor, not an instruction to migrate every subsystem at once.
The current production path remains described in
[DRM rendering improvement plan](drm-rendering-improvement-plan.md).

## Finding

Weld currently uses Smithay below its intended DRM composition boundary. It
builds directly on `GbmBufferedSurface`, then owns swapchain leasing, KMS queue
and retirement policy, cursor-plane commits, retry state, and global frame
scheduling. Smithay's `DrmOutputManager` and `DrmCompositor` already coordinate
these responsibilities per CRTC and across the device.

Smithay does not require ownership of Weld's scene. `DrmOutput::render_frame`
leases its selected primary DMA-BUF and passes it to the renderer through
`Bind<Dmabuf>`. A wgpu-backed implementation of that seam can render directly
into the Smithay-owned allocation. This requires neither a CPU pixel copy nor
an output-sized GPU copy and does not require a new Smithay API.

## Ownership boundary

| Responsibility | Intended owner | Reason |
| --- | --- | --- |
| libseat session and DRM device lifecycle | Smithay | Existing backend integration already models pause and activation. |
| Connector, CRTC, mode, and atomic KMS state | Smithay | `DrmOutputManager` coordinates output changes and device-wide bandwidth constraints. |
| Primary swapchains and framebuffer export | Smithay | `DrmCompositor` allocates, tests, queues, and retires buffers per CRTC. |
| Plane assignment, damage clips, modifier fallback, and direct scanout | Smithay | These are coupled to KMS capabilities and atomic tests. |
| Page-flip metadata and per-output pacing input | Smithay host boundary | The callback identifies the CRTC, timestamp, and sequence; Weld may choose policy from that data. |
| Vulkan DMA-BUF import and wgpu target view | Narrow Weld renderer adapter | This is the unavoidable wgpu interop seam. |
| Bevy scene, client-surface composition, and target selection | Weld | These define Weld's programmable compositor and headless or streaming behavior. |
| Window entities and window-management policy | Weld plugins | Smithay protocol objects are inputs, not Weld's application model. |

Wayland protocol state already implemented through Smithay remains in
`weld-core`. Smithay desktop helpers such as `Space` and `Window` must not
become a second source of truth beside Weld's entity and window primitives.

## Probe

`smithay_drm_compositor_probe` deliberately implements only enough of
Smithay's renderer traits to draw a changing solid color through wgpu. It
validates this sequence on one physical output:

```text
DrmOutputManager selects and leases a GBM primary buffer
    -> Bind<Dmabuf> rejects implicit modifiers
    -> Vulkan transfers foreign ownership to the wgpu queue
    -> wgpu clears the selected image directly
    -> Vulkan returns the image to foreign ownership
    -> the probe waits for that release submission
    -> DrmOutput queues the completed buffer
    -> the matching CRTC page flip retires that frame
```

A successful run must report the selected FourCC and explicit modifier. An
`Invalid` modifier is a hard failure: wgpu cannot safely infer the allocation
layout, so accepting Smithay's implicit-modifier compatibility fallback would
not prove this boundary. The foreign release must also be GPU-complete before
the probe returns a signaled Smithay synchronization point.

The probe contains no client renderer, Bevy scene, cursor, multi-output
policy, direct scanout candidate, or production fence export. Weld's current
presenter already proves that Bevy can render into an imported GBM image. This
experiment proves the inverse ownership direction: Smithay may own the output
allocation and KMS lifecycle without taking ownership of Bevy composition.
Its blocking GPU wait is intentionally diagnostic; production should export a
native completion fence instead.

`scripts/run-smithay-drm-compositor-probe` requires a real TTY and enables both
the Khronos validation layer and Vulkan synchronization validation. It verifies
that the layer loaded and rejects reported layout, lifetime, or synchronization
hazards before treating the run as evidence.

### Validated evidence

The AMD/RADV TTY run on August 22, 2026 retired 2,072 frames at approximately
the 60 Hz output cadence. Smithay selected explicit-modifier ARGB8888, paused
cleanly for a VT switch, activated the same output, forced a fresh full frame,
and resumed normal page-flip retirement. Validation reported no VUID, image
layout, lifetime, or synchronization hazard.

The first activation retirement carried sequence zero and a zero monotonic
timestamp. This is a supported DRM metadata case rather than a failed flip:
Smithay's Anvil treats a zero monotonic timestamp as unavailable and uses its
local monotonic clock for presentation feedback. Weld must preserve that
fallback instead of publishing time zero.

The probe keeps its Vulkan capability query, foreign-ownership barrier, and
import cache local. This intentionally duplicates a small amount of proven
interop code so validation does not refactor the production presenter before
the ownership seam succeeds on a real TTY. Those copies become deletion or
extraction candidates only during the later production migration.

`DrmCompositor` caches one exported `Dmabuf` in each swapchain slot. The probe
therefore keys wgpu imports by Smithay's `WeakDmabuf` identity and evicts an
entry once that slot-owned handle is gone. This is simpler and safer than
reconstructing allocation identity from file metadata, and avoids stale
pointer reuse by pruning dead weak keys before every lookup.

VT activation relies only on `DrmOutputManager::activate`. It resets each
compositor's state and forces a full frame until one is queued successfully.
Resetting buffer ages again is redundant; it may replace a still-referenced
slot and discard Smithay's cached buffer metadata. The probe also clears the
entire framebuffer on every rendered frame, so its result does not depend on
buffer age and does not validate partial-damage rendering.

Initiating a VT switch must stop physical queueing before calling `change_vt`.
DRM access may be revoked before calloop delivers `PauseSession`, while a final
page flip can still retire in that interval. Production Weld additionally
suspends its presenters and pauses the DRM device synchronously before the
request, then tolerates the repeated pause notification. The focused probe
closes its queueing gate immediately and leaves the single device pause to its
`PauseSession` handler.

## Production use of Smithay

The validated minimal seam is `DrmOutputManager` plus a Weld renderer that
implements `Renderer` and `Bind<Dmabuf>`. Nourish independently uses the same
public boundary with its Vulkan renderer. Adding an external-primary-buffer
API to Smithay or retaining Weld's low-level `GbmBufferedSurface` presenter
would duplicate machinery already behind that seam.

Production should additionally use Smithay facilities that the focused probe
does not exercise:

- pass the real Smithay `Output` as `OutputModeSource::Auto`, so mode, scale,
  and transform changes remain one output state;
- maintain `DrmOutputRenderElements` for device-wide operations that may need
  to recompose another output;
- consume per-CRTC page-flip metadata for pacing and presentation feedback,
  falling back to the local monotonic clock when DRM reports zero time;
- expose accurate element commit and damage state to Smithay's output damage
  tracker as Bevy gains retained rendering support;
- use `FrameFlags::DEFAULT` only when eligible plane elements are supplied;
  the probe uses `FrameFlags::empty()` and validates primary composition only;
- provide a `Kind::Cursor` element backed by stable memory and a GBM cursor
  device so Smithay can retain, reposition, and commit the hardware cursor
  plane; keep Weld's GPU cursor as the capability fallback;
- expose eligible unadorned client buffers as separate elements when direct
  scanout or overlay promotion can bypass the Bevy scene without duplicating
  the client;
- return an exportable `SyncPoint` when wgpu can provide a native completion
  fence. The probe deliberately waits for GPU completion and returns an
  already-signaled point.

Smithay's optional `backend_vulkan` module is not useful for this boundary. It
creates its own Vulkan instance and offers physical-device queries, but no
renderer or external-memory importer. Enabling it would not replace wgpu's
adapter, DMA-BUF import, or foreign queue-family ownership work.

Weld continues to own Bevy scene composition, client-buffer-to-Bevy imports,
window and plugin policy, and selection between physical, retained, capture,
headless, and streaming targets. These are compositor-product concerns rather
than DRM output mechanics.

## What the target seam subsumes

Adopting this seam would let Weld remove rather than preserve parallel output
machinery:

- the custom `GbmBufferedSurface` presenter and global physical-frame tracker;
- manual primary-buffer queueing and state-only vblank retirement;
- custom device-wide output batching and modifier fallback policy;
- custom atomic cursor submission and coalescing built into the low-level GBM
  surface;
- retry and reset state that duplicates `DrmOutputManager` lifecycle behavior.

The vendored `GbmBufferedSurface::clear_pending_scanout` patch and its atomic
cursor, cursor-deferral, and state-only-vblank patches are therefore deletion
candidates with that production path. The cursor-deferral work is currently an
uncommitted, state-dependent local change and must not be removed before the
production migration. `DrmDeviceFd::new_unprivileged` is orthogonal: render
nodes still need it for explicit-sync imports without DRM-master behavior.

The next pass may aggressively delete these candidates, but it must retain a
small core for Wayland surface lifecycle, DMA-BUF-to-Bevy textures, output
target binding, page-flip-driven scheduling, window primitives, and protocol
dispatch. Headless composition remains an independent target selected by Weld,
not a fallback owned by KMS.
