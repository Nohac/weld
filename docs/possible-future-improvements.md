# Possible future improvements

**Status: Exploration.** This note records implementation ideas worth revisiting
after comparing Weld with [Nourish](https://github.com/y5-snowies/nourish). It is
not a roadmap, compatibility target, or implementation checklist. The subject
[specifications](spec/README.md) remain the source for Weld's direction, while
[Architecture](architecture.md) records what the repository actually implements.
Any item taken forward must first be checked against Weld's pinned Smithay tree,
Bevy version, and supported hardware.

## Boundary to preserve

Nourish and Weld place their primary scene graph on opposite sides of the
renderer boundary. Nourish composes Smithay render elements with a custom Vulkan
renderer and can feed Bevy or Iced output into that composition through
DMA-BUFs. Weld imports client surfaces into Bevy so client content, shell UI,
effects, clipping, and transforms remain ordinary Bevy composition.

The useful parts of Nourish are therefore lifecycle, synchronization, output,
and interop techniques—not its primary scene hierarchy or renderer wholesale.
Weld plugins should continue to see Weld and Bevy types rather than Smithay,
wgpu-hal, Vulkan, KMS, or DMA-BUF details.

## Candidate investigations

### Smithay-managed output buffers beyond the first direct target

Weld now lets DRM/GBM allocate scanout buffers and binds each leased image
directly to Bevy's stable output target while the physical output is active.
The retained application texture takes over when that output is inactive or a
capture requires owned storage. Remaining investigations include independent
multi-output targets, native completion fences, damage clips, simultaneous
streaming consumers, VRR, HDR, overlay promotion, and
fullscreen client direct scanout.

The current Vulkan ownership and modifier path still needs validation across
AMD, Intel, and NVIDIA drivers before portability claims are justified. See
[Rendering](spec/rendering.md), [Direct DRM presentation](drm-presentation.md),
and the [DRM rendering improvement plan](drm-rendering-improvement-plan.md).

### Output-owned presentation state

Model each physical output as an independently recoverable presentation state
machine. Its output identity, CRTC, connector, mode, scale, composition target,
camera, scanout buffers, in-flight frame, damage, presentation timing, color
state, and protocol global should have one clear lifetime.

Important invariants include:

- Route vblank only to the output owning the reported CRTC. Ignore a late
  event for a removed output instead of falling back to a primary output.
- Pace each output from its own refresh and presentation history so one display
  cannot accidentally throttle another.
- Retire output-scoped GPU resources and the matching `wl_output` state
  together on removal or mode replacement.
- Keep logical windows and policy stable while their presentation moves
  between output cameras and targets.

Windows that cross an output boundary need one presentation projection for
each intersected output. Each projection targets that output's camera and uses
its scale, clipping, transform, color state, and composition target; Weld-owned
SSD, shadows, and other UI are therefore rendered independently per output.
The window keeps one authoritative home output for policy, while output
intersections and projection entities are derived state rather than competing
window ownership.

The client surface does not need a separate Wayland buffer for every
projection. When overlapping outputs use different scales, Weld should prefer
one client buffer rendered at the highest useful intersected scale and sample
it into every output projection, downscaling where necessary. This keeps one
temporally coherent surface state across outputs. Retaining buffers from
successive preferred-scale changes would create stale per-output versions,
delay buffer release, and encourage redraw feedback near output boundaries.
Preferred-scale changes should use hysteresis so small boundary movements do
not repeatedly force the client to re-render.

### Native explicit-sync release fences

Weld now capability-gates `linux-drm-syncobj-v1`, blocks commits on explicit
acquire points, and signals each per-commit release point after the existing
wgpu completion worker proves that use has retired. The current release signal
is a CPU syncobj ioctl on the server thread after the off-thread wait; it is
correct and does not block the compositor on GPU work.

A later optimization can export the Vulkan release submission as a native
sync-file fence and import that fence directly into the client's release point.
That would let the kernel carry the dependency without the CPU completion
round-trip. It must preserve the implicit client path, per-use release
identities, never-sampled immediate releases, and the lifetime pins held by the
current completion work. It should be driven by measurements rather than
treated as a prerequisite for multi-output presentation.

Explicit acquire event sources should also gain bounded cleanup tied to their
surface or client lifetime. A signaled point removes its own calloop source,
but a broken or malicious client can currently leave an unsignaled eventfd
registered after abandoning the surface. Fixing that requires explicit
registration-token ownership; it should not be approximated with polling or a
generic frame timeout.

### Multi-plane DMA-BUF import

Weld's current direct-sampling path deliberately accepts one-plane formats.
Supporting modifier layouts such as AMD DCC or Intel CCS may require importing
multiple DRM planes into one Vulkan image and extending the temporary wgpu-hal
interop patch if upstream wgpu still lacks the needed API.

Nourish demonstrates that such a patch is feasible, but its assumption that
all planes share the first file descriptor must not be copied without proof.
Weld should validate shared backing and plane metadata, explicitly support or
reject disjoint allocations, and advertise only format/modifier combinations
that have passed a real import probe. YUV and other genuinely multi-image
formats remain a separate design problem.

### Bounded off-thread GPU producers

Some future features—remote encoders and decoders, isolated plugin scenes, or
headless consumers—may benefit from a worker rendering into a bounded DMA-BUF
ring while the compositor samples the newest completed slot. Useful properties
from Nourish's worker design are blocking while idle, coalescing redundant tick
requests, bounding queues toward the latest frame, and explicitly waking
calloop when publication makes new damage visible.

Publication needs a monotonic generation that survives ring rebuilds so a
resize cannot look like an old frame. GPU objects must remain pinned through
their final in-flight use. If a non-`Send` Bevy `App` is involved, construct it
on its owning worker from sendable configuration rather than moving it between
threads. This is not a reason to move Weld's main shell application or raw
client input path off the compositor thread.

### Native startup environment hygiene

When DRM was selected before Vulkan initialization, Nourish removes inherited
`WAYLAND_DISPLAY` and `DISPLAY` values so Mesa device-selection layers cannot
connect to a stale desktop socket during concurrent graphics startup. Weld
should investigate the same narrowly scoped precaution if startup traces show
that behavior. Nested mode must retain its host environment, and launched
clients must still receive Weld's private Wayland socket explicitly.

### Fail-soft display and GPU recovery

Output sleep, unplug, session pause, late vblank, and recoverable presentation
errors should affect only the relevant output. The Wayland control plane and
headless or streaming composition should remain alive during a zero-output
period. Recovery should be an event-driven state transition that rebuilds only
resources whose capabilities were lost.

Watchdogs are appropriate only for a verified driver stall or a narrowly
bounded first-frame/resume failure. Timers and polling should not replace
libseat, udev, vblank, GPU-completion, or presenter-channel events. Device loss
that invalidates all graphics ownership remains distinct from an ordinary
output lifecycle event. See [Platform completeness](spec/platform-completeness.md).

### Cache and pacing invariants

The comparison also reinforces several smaller rules for future renderer work:

- Cache imported scanout targets by allocation identity and let dead buffers
  retire without a cache entry keeping them alive forever.
- Separately pin every resource used by an in-flight GPU submission.
- Wake the host explicitly when off-thread publication changes what can be
  composed; damage tracking cannot observe unpublished worker state.
- Derive recurring deadlines from the prior deadline rather than completion
  time to avoid pacing drift.
- Do not compare or combine timestamps from clocks whose epochs have not been
  proven compatible.

Weld already applies the corresponding source-buffer lifetime principles in
its DMA-BUF manager. These are constraints for extending the design, not a
proposal to replace the current direct-sampling path.

## Ideas intentionally not adopted

- Smithay render elements do not become Weld's public or primary scene graph.
- A custom Vulkan renderer does not replace Bevy/wgpu merely because Nourish
  has one.
- Pixman or GLES must not become a silent fallback that makes an advertised
  fast path appear functional.
- Ordinary scenes do not each receive a separate Bevy application or worker
  thread.
- Broad global atomics, locks, timers, and generated micro-crates are not an
  architecture pattern to copy.
- Direct scanout or hardware-plane promotion must remain optional; normal Bevy
  composition is the correctness path whenever shell UI or effects are visible.

## Suggested investigation order

1. Multi-output hotplug, independent vblank routing, per-output presentation,
   and fail-soft recovery.
2. Native explicit-sync release fences if profiling shows the CPU completion
   round-trip matters.
3. Multi-plane import with exact modifier probing and validation.
4. Off-thread producer rings only when streaming or isolated rendering has a
   concrete consumer.
