# Direct DRM presentation

This note records DRM presentation evidence, ownership rules, and recovery
requirements. The production backend now renders Bevy directly into Smithay's
GBM/KMS buffers while a physical output is active. The retained Vulkan Display WSI probe at
`crates/weld-core/examples/drm_wsi_probe.rs` is intentionally independent from
the production compositor, so it remains useful for isolating driver, session,
and VT behavior.

## Current direction

Smithay owns the DRM device, connector/CRTC state, GBM swapchain, page flips,
and vblank retirement. Weld uses `GbmBufferedSurface` as the scanout allocator
and KMS sink, but does not use Smithay render elements or a Smithay renderer.
wgpu imports each leased scanout DMA-BUF, Bevy renders the full scene directly
into it, and a scissored pass overlays the compositor cursor before ownership
returns to KMS. There is no intermediate full-output texture or scene blit on
the active physical path.

`weld-app` retains an owned texture behind the same stable Bevy camera handle.
That texture becomes the destination while the VT/output is inactive or a
capture requires owned storage. Returning to the output requests a fresh
direct composition instead of copying the retained image into scanout. The
staged work and remaining follow-ups are tracked in the
[DRM rendering improvement plan](drm-rendering-improvement-plan.md).

The initial target is modern Vulkan hardware, one GPU, and one output. This
does not establish singleton presenter or output APIs that would prevent later
multi-output or multi-GPU work.

Weld temporarily patches its Bevy 0.19 rendering crates to wgpu 30. Version 30
adds the initial resource state to `Device::create_texture_from_hal`; the
linux-dmabuf importer uses it to adopt an initialized external Vulkan image
without a discard transition. The same device is opened with
`VK_EXT_queue_family_foreign` so the importer can acquire the image from and
release it back to the Wayland producer. This dependency override must be
removed, together with `vendor/bevy-wgpu30`, when Weld adopts a suitable Bevy
release with native wgpu 30 or newer support. The raw barriers and import path
remain necessary unless a future safe wgpu API owns those transitions too.

DMA-BUF import is independent of physical presentation and remains active while
the VT or connector is unavailable. A persistent GPU-completion worker waits
for retiring-image release barriers and returns only numeric release identities
through a wakeable calloop channel. It does not poll, use deadlines, carry
Wayland objects, or submit Vulkan work. This lets clients reuse buffers while
Weld continues demand-driven headless composition during a session pause.

The protocol acknowledges a buffer only after the shared Vulkan source cache
has imported it successfully. A live `wl_buffer` owns that cached native image
until destruction, while per-commit GPU completions are reference-counted so
the server does not release a reused buffer early. The initial foreign acquire
assumes the producer presents initialized images in `GENERAL`; Weld's release
barrier makes that layout exact for all later uses of the cached import.

That native import, its Bevy image identity, and warmed material bind groups
are stable for the lifetime of the `wl_buffer`. Commits create presentation
leases, not new GPU images. A client buffer-pool rotation therefore reuses its
already prepared bindings after warm-up. Acquires for one composition are
recorded into one barrier command buffer. Release submission remains
unconditional when a protocol lease retires—even if a shared image needs no
ownership transition—because its submission index is the fence that orders
`wl_buffer.release` after Bevy's preceding reads.

### Direct sampling and wgpu dependency

The current DMA-BUF path is end-to-end zero-copy for client surface content.
Weld imports the client's allocation as an external Vulkan image and Bevy's
surface material samples it directly into the final composition. There is no
intermediate client-sized texture or normalization pass.

This requires retaining the displayed `wl_buffer` rather than releasing it
after one copy. A newly committed buffer is staged until the corresponding ECS
image is ready. Weld acquires it before Bevy's next submission, retains it in
render-queue ownership across unchanged redraws, and retires its predecessor
only after that same RenderApp run can no longer reference the predecessor.
The post-Bevy release barrier is ordered after every possible old-image read;
`wl_buffer.release` follows its GPU completion. A staged buffer superseded
before promotion was never read and can be released immediately after queued
ECS state has drained. Acquisition is reference-counted by imported Vulkan
image, not by ECS layer identity. Reattaching one buffer or displaying the same
buffer in multiple layers therefore performs one 0-to-1 acquire and one final
1-to-0 release while preserving a completion identity for every protocol use.

Normalization is fused into composition. Pixel-aligned 1:1 samples normally
need one texel load. Scaled, rotated, or subpixel samples use four loads so each
encoded premultiplied texel can be unpremultiplied and converted to linear color
before interpolation. Those taps clamp to the full texture, not the viewport
crop. This scaled translucent path is more expensive per composition sample
than an ordinary hardware-filtered Bevy image; the intended win is removal of
the full-surface source read, intermediate write, and intermediate reread on
every client commit. Profile ARGB Firefox content specifically rather than
assuming the opaque path represents the workload.

The temporary wgpu 30 pin is therefore still required. Weld passes the known
initial resource state when adopting the initialized Vulkan image through
`Device::create_texture_from_hal`; the Bevy 0.19 wgpu generation does not
provide the required contract. Remove the pin only after Weld adopts a Bevy
release with native wgpu 30 or newer support, or replaces this import path with
another design that preserves external-image state and ownership correctly.

## Deferred performance baseline

Performance was sampled on the earlier GPU-normalization-blit checkpoint using
an AMD Strix integrated GPU (Radeon 880M/890M) while three YouTube videos played
in three separate Firefox windows. This is a diagnostic before-baseline, not a
cross-hardware target or a measurement of the direct-sampling path.

With playback stopped, Weld used approximately 0–1% CPU, aggregate GPU busy
was 6–7%, and the AMD driver reported roughly 10–13 W PPT. With all three
videos playing, the stable ranges were:

- Weld CPU: 12–14%;
- Weld's per-process graphics-engine busy time: 14–16%;
- selected Firefox video and media processes: 38–45% CPU;
- Firefox graphics-engine busy time: about 4%;
- Firefox RDD media-engine busy time: about 5%;
- aggregate GPU busy: 18–20%;
- AMD PPT: approximately 20–23 W;
- GPU edge temperature: 58–60°C;
- APU temperature: usually 71–75°C.

Firefox's RDD process accumulated AMD media-engine time throughout playback,
so hardware media acceleration was active even though this kernel's aggregate
`vcn_busy_percent` remained zero. Do not use that one aggregate counter as the
sole decoder signal.

Weld's main compositor thread accounted for roughly 9–12% CPU. The DRM
presenter used about 1–2%, and the DMA-BUF completion worker used 0–1%. The
trace that produced these figures motivated the stable image/bind-group cache
and barrier batching described above. Re-measure before attributing the
remaining cost to frame orchestration or Bevy composition frequency. Switching
away from Weld did not remove this workload:
demand-driven headless composition intentionally continues while the VT is
inactive so clients and future streaming consumers keep progressing.

The first informal observation after direct sampling replaced the normalization
blit was approximately 10–13% Weld CPU with three videos playing in separate
Firefox windows. Rapid pointer movement and window dragging did not push Weld
past approximately 20–21%. Laptop temperatures were roughly 70–80°C, but no
instrumented GPU counters were recorded. Repeating the three-video experiment
under Sway produced approximately 5–7% CPU and a temperature about 5°C lower.
These are informal observations rather than controlled samples, but they show
that direct sampling removes a substantial cost without yet reaching Sway's
frame-orchestration and composition efficiency.

### Reproducing the profile

Use intervals of at least 30 seconds and record the active VT so visible and
headless samples are not confused. Compare the same client state with playback
stopped, playing on Weld's active VT, and playing after switching away.

1. Identify the GPU and Weld process:

   ```sh
   lspci -nnk | rg -A4 -i 'vga|3d|display'
   ps -eo pid,ppid,psr,pcpu,stat,comm,args | rg 'weldwm|firefox'
   cat /sys/class/tty/tty0/active
   ```

2. Resolve the matching DRM card and its hwmon directory, then sample the
   driver's aggregate counters:

   ```sh
   cat /sys/class/drm/card1/device/gpu_busy_percent
   cat /sys/class/drm/card1/device/vcn_busy_percent
   cat /sys/class/drm/card1/device/hwmon/hwmon*/power1_average
   cat /sys/class/drm/card1/device/hwmon/hwmon*/temp1_input
   cat /sys/class/drm/card1/device/hwmon/hwmon*/freq1_input
   ```

   Card and hwmon indices are machine-specific. `power1_average` and
   temperatures use microwatts and millidegrees Celsius respectively.

3. Inspect per-process DRM contexts through `/proc/<pid>/fdinfo/*`:

   ```sh
   rg 'drm-driver|drm-engine|drm-total|drm-resident' /proc/<pid>/fdinfo/*
   ```

   Take counter deltas across a timed interval. Multiple file descriptors can
   expose the same DRM context and identical cumulative counters, so choose one
   descriptor per context rather than summing duplicates.

4. Separate compositor thread costs:

   ```sh
   ps -T -p <weld-pid> -o pid=,tid=,psr=,pcpu=,stat=,comm= --sort=-pcpu
   top -H -b -d 1 -n 10 -p <weld-pid>
   ```

When profiling resumes, add rate and duration counters for DMA-BUF commits,
one-tap and four-tap surface samples, Bevy compositions, physical
presentations, frame callbacks, import preparation, ECS advancement, rendering,
and presentation. Then compare the
development binary with a release build. Static release linking and higher
optimization may reduce main-thread CPU, but should not be assumed to reduce
GPU composition or memory bandwidth without measurements.

## Historical Display WSI probe evidence

The probe has presented directly through wgpu on AMD RADV using:

- `/dev/dri/card1`, connector `eDP-1`;
- Vulkan display plane index 0;
- 2240 by 1400 at 60002 mHz;
- `Bgra8UnormSrgb` with FIFO presentation.

These results validate the separate probe, not the production GBM/KMS sink.

The probe uses `DrmDeviceFd` and `DrmScanner` directly. Avoiding Smithay's
`DrmDevice` isolates the WSI experiment from Smithay's atomic KMS snapshot and
restoration lifecycle. Production deliberately makes Smithay's `DrmDevice` the
sole KMS owner instead.

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

The DRM-only virtual-terminal shortcut plugin maps `Ctrl+Alt+F1` through
`Ctrl+Alt+F10` to one-shot ECS requests. The DRM host suspends the presenter
before passing a request to libseat. Nested mode does not install this plugin
and has no VT-switch request path.

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

The calloop thread owns every Smithay and KMS object. It leases a GBM buffer,
safely imports and acquires it, and gives its wgpu view to Bevy immediately
before composition. After Bevy submits, the same calloop thread records the
cursor overlay and foreign release. It sends only a `SubmissionIndex` and frame
ticket to the worker, then queues the buffer through `GbmBufferedSurface` after
the worker reports GPU completion. The matching CRTC vblank retires the frame.
Worker results return through a wakeable calloop channel.

Only one physical frame is active. Its phase is rendering or awaiting vblank,
while application `FrameState` coalesces newer demand. Session epochs invalidate
stale work. If a VT activates while the worker still waits on a leased buffer, activation
waits for that terminal worker event before clearing Smithay's stale scanout
state or reusing the slot. The event channel is the normal wakeup; a bounded
frame-interval timeout prevents a missing result from causing a permanent sleep
or a zero-timeout spin.

Transient presentation errors have a bounded reset budget and retry the newest
retained frame. A retired vblank replenishes that budget. An exhausted budget,
a failed reset, device loss, or a failed DRM event source disables only physical
presentation; the event source failure requires restart because Weld can no
longer retire page flips. Scanout construction also rejects implicit DRM
modifiers before direct composition starts, rather than failing on the first Vulkan
import.

The Display WSI probe's short calloop service interval and missing-event
deadline remain diagnostic instrumentation for its intentionally
single-threaded form. They are not production scheduling policy.

Shutdown order is part of the KMS ownership contract. The presenter and its
`DrmSurface` retire first, then Weld removes the registered DRM notifier and
pauses `DrmDevice` before dropping it. Pausing deliberately suppresses
Smithay's generic previous-state replay: that snapshot can contain file-scoped
framebuffer and property-blob identifiers owned by the compositor that ran
before Weld, so replaying it through Weld's DRM file is invalid. The display
can remain on Weld's last scanout until libseat or logind switches the VT and
the receiving session performs its own modeset; this is the same observable
fallback as a failed snapshot replay, without issuing the invalid commit.
Calloop's libseat notifier remains the actual seat owner until device and fd
teardown completes. A host-loop RAII guard preserves that ordering on normal
return, early errors, and panic unwinding.

Physical presentation availability and composition policy remain separate.
The initial standalone implementation keeps demand-driven composition active
while its VT or connector is unavailable so client frame callbacks continue to
make progress. It does not run a free-running refresh timer without a capture,
stream, client commit, Bevy redraw, or other composition consumer.

The output refresh rate bounds presentation opportunities for continuous work;
it does not force every client to commit at that rate. Clients may update more
slowly, and idle or occluded clients should reuse their retained buffers.
Multi-output refresh differences, VRR, exclusive fullscreen scanout, and
headless streaming introduce their own consumer cadences without changing the
DMA-BUF ownership rule.

## Composition ownership

`weld-app` owns one retained texture and one stable manual-view handle. DRM
selects `Bgra8UnormSrgb` for that texture because its leased scanout views use
the same format. Switching between them therefore changes neither the camera
target identity nor Bevy's pipeline specialization key.

For a physical frame, core acquires the leased image first and passes its view
as an external destination. The shared wgpu queue then orders the raw Vulkan
acquire, Bevy's render submissions, the scissored cursor pass, and the raw
foreign release. The output camera performs an opaque full-target clear before
the cursor pass uses `LoadOp::Load`; changing that camera to a non-writing clear
mode would violate the initialization contract for a fresh imported buffer.
The completion worker waits only for the final release submission. Tickets
carry presenter generation, session epoch, and frame identity so late results
cannot release newer work.

Direct composition is serialized behind scanout availability. This removes the
old full-output blit but also removes the previous overlap where Bevy rendered
into a second offscreen target while the worker prepared the first. Cursor
motion requests one refresh-capped composition rather than re-presenting a
retained scene independently; a KMS cursor plane remains the route to decouple
cursor latency from Bevy composition.

When no physical target is usable, composition selects the retained texture and
continues demand-driven. A screenshot also selects that target and reads it
before requesting the next direct physical frame. DRM captures now contain the
opaque output background formerly supplied by the removed blit, but still omit
the cursor. Nested captures likewise omit the host-system cursor.

Nested and direct presentation deliberately complete client frame callbacks
at different boundaries. The nested backend completes them after its host
surface accepts the present. Direct DRM completes them after Bevy composition,
independent of whether a physical output currently accepts that frame. This is
what keeps clients live through VT switches and output loss; it is not a claim
of exact scanout timing.

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

Only the active state permits physical presentation. Composition stays
available in every state and remains demand-driven when there is no physical,
capture, or streaming consumer.

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

wgpu reports uncaptured errors and device loss through callbacks shared with
Bevy. Weld forwards them to the host so physical presentation can become
unavailable without treating a recoverable Rust error as a compositor panic.

On a connector or GPU change, the host retains logical output and window state,
stops sending work to the affected presenter, and reprobes from udev evidence.
A replacement presenter uses a new generation only after the previous worker
and imported output buffers have been safely retired. If a native driver blocks
indefinitely or aborts, stronger fault containment still requires a future
presentation-process boundary; ordinary Rust and device errors remain
recoverable in process.

The direct backend is not considered robust until it passes repeated and
long-duration tests covering monitor power off and wake, cable unplug and
reconnect, mode and EDID changes, VT switching, system suspend and resume, GPU
device loss, failed surface acquisition, and clean shutdown while an output is
unavailable. These tests must verify that Wayland clients remain connected and
that the compositor either restores physical presentation or remains usable in
its non-physical composition mode.
