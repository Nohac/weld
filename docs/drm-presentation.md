# Direct DRM presentation

This note records evidence and architectural decisions for replacing Weld's
transitional CPU-copy DRM renderer with direct wgpu presentation. The retained
probe at `crates/weld-core/examples/drm_wsi_probe.rs` is intentionally
independent from the production compositor, so it remains useful for isolating
driver, session, and VT behavior.

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
next investigation should therefore begin with frame orchestration, Bevy
composition frequency, and per-frame DMA-BUF command preparation rather than
completion waiting. Switching away from Weld did not remove this workload:
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

The host-side presenter lifecycle should be represented as one explicit state
machine once the first hardware integration has validated the real event
ordering. Its states need to cover configuring, ready, session-suspended,
connector-unavailable, mode-incompatible, device-lost, stopping, and stopped.
Only the cross-thread interruption facts remain atomic: whether acquisition is
allowed and the epoch that invalidates work already blocking in the driver.
Readiness, retry budget, shutdown, and output compatibility belong to the host
state transition rather than independent booleans.

Shutdown needs an explicit lifecycle rather than a timeout disguised as
coordination. In particular, the worker may still own wgpu objects while
blocked, yet the libseat-owned DRM fd must remain alive until the wgpu instance
is dropped. This ownership and termination rule is a design gate for the
production presenter.

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

Bevy renders into two project-owned composition targets. Target identity and
the latest presenter-owned cursor overlay are part of every presenter frame
request; target identity is repeated in the result. The host never renders
into a target while the presentation worker owns it. While FIFO acquisition
blocks for one target, newer state may be rendered and coalesced into the
other target without growing a frame queue. Cursor-only motion offers the
completed target again with updated overlay metadata instead of dirtying Bevy
composition. The pending slot retains only the newest complete composition and
cursor payload.

Every terminal presenter result releases its target, including deferred,
interrupted, unavailable-output, device-loss, worker-stop, and panic outcomes.
The result carries the presenter generation, target identity, and frame
identity so a late result cannot release a target now owned by newer work.
After the worker submits its blit and calls present, its release event crosses
the channel before the host can submit another Bevy write to that target. Both
operations use the same wgpu queue, so the later write is ordered after the
blit read.

The worker owns one cursor uniform shared across its submissions and rewrites
it immediately before submitting the matching frame. Its one-in-flight queue
ordering prevents a later cursor payload from overtaking that write. Nested
presentation leaves the same uniform hidden and relies on its host cursor.

Resizing or replacing the targets first advances their generation. The host
then waits for or invalidates any outstanding ownership before dropping and
recreating both textures. A screenshot reads the completed target associated
with the requested frame. Its copy submission uses the same queue and retains
that target until readback has been ordered, preventing a later composition
from overwriting it first. Captures omit the presenter or host cursor in both
DRM and nested modes.

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

`Surface::configure` is contained against synchronous Rust panics. wgpu reports
non-panicking validation failures through the device-global uncaptured-error
callback shared with Bevy, so the worker cannot safely attribute such an error
to configuration alone. Weld logs that shared-device error, and the bounded
next acquisition determines whether physical presentation becomes unavailable.

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
