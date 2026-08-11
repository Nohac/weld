# Direct DRM presentation

This note records evidence and architectural decisions for replacing Weld's
transitional CPU-copy DRM renderer with direct wgpu presentation. The retained
probe at `examples/drm_wsi_probe.rs` is intentionally independent from the
production compositor so it remains useful for isolating driver, session, and
VT behavior.

## Current direction

Vulkan and wgpu will be the sole presenter for a connector while Weld owns it.
Smithay remains responsible for Wayland protocols, libseat, udev, input, and
surface state, but Weld will not construct Smithay DRM compositor, GBM, or
Pixman renderer objects for presentation. Once the direct path replaces the
transitional backend, the Pixman and GBM renderer features will be removed
without a fallback.

The initial target is modern Vulkan hardware, one GPU, and one output. This
does not establish singleton presenter or output APIs that would prevent later
multi-output or multi-GPU work.

## Validated display path

The probe has presented directly through wgpu on AMD RADV using:

- `/dev/dri/card1`, connector `eDP-1`;
- Vulkan display plane index 0;
- 2240 by 1400 at 60002 mHz;
- `Bgra8UnormSrgb` with FIFO presentation.

The initial production path requires an eight-bit sRGB surface format and FIFO
presentation and fails clearly when either is unavailable. These are explicit
compatibility requirements, not fallback preferences.

Discovery uses `DrmDeviceFd` and `DrmScanner` directly. Avoiding Smithay's
`DrmDevice` also avoids its atomic KMS snapshot and restoration lifecycle,
which conflicts with Vulkan display WSI changing KMS state independently.
The DRM fd must outlive the complete wgpu instance because Mesa may retain and
use that raw fd after surface creation.

wgpu-hal matches the requested display mode exactly. The Smithay-to-Vulkan
refresh tolerance is used only to select the closest Vulkan mode; the exact
Vulkan-reported refresh value must then be passed to
`SurfaceTargetUnsafe::Drm`.

The Vulkan display is acquired exactly once when the wgpu DRM surface is
created. Reconfiguring that existing surface after session activation is
allowed. Recreating it is not: wgpu-hal would invoke
`vkAcquireDrmDisplayEXT` again, and the tested driver rejects the second
acquisition.

## VT-switch evidence

One refined probe run requested a VT switch and then entered FIFO surface
acquisition. Acquisition blocked for 7011 ms and returned `Lost` only when the
user switched back. During that wait, calloop could not process Smithay's
libseat notifier. Smithay acknowledges a libseat disable inside that event
source before delivering `PauseSession`, so seatd and Mesa were effectively
waiting on work owned by the same blocked thread.

That run reported neither `PauseSession` nor `ActivateSession`; it therefore
did not test whether explicit DRM master ioctls are required. A master problem
is implicated only if both events arrive, the existing surface is configured
after activation, and presentation still fails.

A subsequent run observed both events, reconfigured the existing surface, and
presented 1,213 frames after activation before shutting down cleanly. On the
tested libseat, RADV, and connector path, libseat alone restored device access;
explicit DRM master acquisition and release were not required.

The following recovery sequences are valid probe results:

- `PauseSession`, `ActivateSession`, then a successfully presented frame;
- `Lost`, `PauseSession`, `ActivateSession`, then a successfully presented
  frame when an external switch raced acquisition.

An earlier probe recovered by configuring the existing surface immediately
after `Lost`. Configuration after activation is the preferred ordering because
device access has been restored first; immediate configuration remains a
diagnostic fallback if a driver rejects the preferred order.

## Production event boundary

Direct FIFO acquisition must not run on Weld's Wayland and session thread.
wgpu supplies a one-second timeout to each of three sequential waits, but the
tested Mesa display WSI still blocked for 7011 ms. The apparent bound therefore
cannot make same-thread acquisition safe.

Production behavior should be event driven:

- calloop reacts to registered libseat, Wayland, input, udev, and presenter
  channel readiness;
- Bevy composition runs from damage and redraw requests;
- an isolated presentation worker owns surface acquisition and presentation,
  blocks naturally in wgpu, and publishes results through a wakeable channel;
- session availability and presentation commands cross that owned channel
  boundary without carrying Smithay or ECS objects;
- the host thread never synchronously waits for a presentation result or joins
  a worker that may be stuck in acquisition.

The probe's short calloop service interval and missing-event deadline are
diagnostic instrumentation for its intentionally single-threaded form. They
are not production scheduling policy and must not become status polling.

Shutdown needs an explicit lifecycle rather than a timeout disguised as
coordination. In particular, the worker may still own wgpu objects while
blocked, yet the libseat-owned DRM fd must remain alive until the wgpu instance
is dropped. This ownership and termination rule is a design gate for the
production presenter.

Physical presentation availability and composition policy remain separate.
The initial standalone policy may suspend composition while its VT is inactive,
but a later headless or streaming consumer must be able to keep composition
active without a physical DRM presentation.

## Output resilience contract

Monitor power saving, link retraining, hot-unplug, GPU removal, VT switching,
and wgpu surface or device loss are runtime state transitions, not reasons to
panic the compositor. They may temporarily or permanently remove a physical
presentation target, but must not destroy clients, ECS-owned window state, or
future headless and streaming consumers.

The production backend should distinguish at least:

- an output that is active and accepting frames;
- an output intentionally suspended for session or power management;
- an output temporarily unavailable while its connector or presenter is being
  reprobed;
- an output whose presenter generation failed and must be replaced.

Only the active state permits physical presentation. Composition policy is
independent: the initial standalone mode may suspend composition in the other
states, while future headless or streaming consumers may keep it enabled.

State changes are driven by libseat, udev, wgpu callbacks, and presenter
channel readiness. They must not depend on periodic status polling. Pending
presentation work is bounded and coalesced so a sleeping output cannot retain
an unlimited number of frames or callbacks. Every presenter generation has an
identifier, and late results from an older generation are ignored after an
output is suspended, disconnected, or recreated.

Weld must install wgpu uncaptured-error and device-lost callbacks instead of
accepting default panic behavior. Recoverable worker errors and Rust panics
must be reported to the host through the presenter channel, allowing the
physical output to become unavailable while the compositor continues running.
Native driver segmentation faults or process aborts cannot be contained inside
the same process; stronger isolation would require a separate presentation
process and shareable GPU buffers.

On a connector or GPU change, the host retains logical output and window state,
stops sending work to the affected presenter, and reprobes from udev evidence.
A replacement presenter uses a new generation only after the previous wgpu
instance has been safely dropped. If a driver leaves the old worker stuck in
acquisition, shutdown and replacement must preserve the DRM-fd lifetime rather
than closing it underneath the driver.

An in-process worker that remains stuck cannot safely be replaced on the same
connector because the old Vulkan instance still owns the display. Weld must
then keep that output unavailable and continue without physical presentation
until process restart. Automatic recovery from that condition requires the
future presentation-process boundary so the failed presenter and its fd can be
terminated together without taking down the compositor host.

The direct backend is not considered robust until it passes repeated and
long-duration tests covering monitor power off and wake, cable unplug and
reconnect, mode and EDID changes, VT switching, system suspend and resume, GPU
device loss, failed surface acquisition, and clean shutdown while an output is
unavailable. These tests must verify that Wayland clients remain connected and
that the compositor either restores physical presentation or remains usable in
its non-physical composition mode.
