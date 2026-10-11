# Direct DRM presentation

Weld has a production-shaped startup multi-output DRM backend built on
Smithay's `DrmOutputManager` and `DrmOutput`. Its Weld-owned layout, pacing, and
batching policies are covered by automated tests. Real-TTY multi-output
validation is still required before its behavior is recorded as hardware
evidence.

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

All connected desktop connectors with a usable CRTC and mode are enabled at
startup. Connectors carrying the DRM `non-desktop` property are rejected. A
stable internal-first/name ordering chooses the primary and output IDs; every
connector uses its preferred mode when available. The primary is centered below
the row of remaining outputs. Runtime scaling targets the output under the
pointer, recomputes the complete logical topology, and never resizes leased
scanout buffers. Hotplug remains deferred.

## Composition and synchronization

Each output contains one stable, opaque Smithay render element representing its
Bevy camera projection. A changed scene increments the relevant element commit
counters. Smithay prepares all due leased views first; Weld then binds them to
their stable Bevy manual targets and runs one RenderApp pass for that subset.
Post-composition cursor fallback and foreign release commands follow on the
same queue. There is no CPU pixel copy, normalization texture, or output-sized
GPU blit.

The import cache follows Smithay swapchain-slot lifetime through `WeakDmabuf`.
Binding an unchanged slot does not acquire it. Foreign queue-family acquisition
happens only when Smithay creates a renderer frame. Bevy submits its work before
the foreign release barrier; the initial implementation waits for that release
submission and returns a signalled Smithay synchronization point. A native
completion fence should later replace the blocking wait without changing the
ownership contract.

At most one physical frame per output is admitted at once. A render request
arriving while that output is queued is retained and re-armed after its matching
vblank rather than being cleared with the older frame. `EmptyFrame` never
creates an in-flight record, and device-wide `DeviceInactive` redirects work to
owned targets. Other render or queue failures quarantine only the affected
output until restart.

Client frame callbacks complete after successful queue submission, giving the
client time to prepare its next buffer before the next composition. Physical
admission remains held until page-flip completion, and GPU buffer-use leases
follow GPU completion. Pending input and configure events are flushed before
entering native presentation; callbacks generated there are flushed afterward.

Each matching CRTC vblank is that output's pacing clock. Outputs are admitted
independently and share a Bevy pass only when their actual deadlines align; for
example, a 120 Hz output can receive an intermediate pass while a phase-aligned
60 Hz output joins every other pass. No harmonic phase relationship is assumed.
Nested and inactive-owned targets retain interval-based pacing because they
have no physical retirement event.

## Cursor

Named and client cursor images are normalized into Smithay
`MemoryRenderBuffer`s. Cursor shape names come from `cursor-icon`; Xcursor theme
inheritance and parsing come from the `xcursor` crate. The ordinary normalized
form is premultiplied `Argb8888`, has a normal transform, an identity source,
an explicit logical size, and integer physical placement.

Weld supplies a locally projected cursor element to every output and enables only
`ALLOW_CURSOR_PLANE_SCANOUT`. Smithay decides whether to copy it into the GBM
cursor buffer or GPU-compose it. Primary and overlay direct scanout remain
disabled. This permits a cursor visual intersecting an output seam to be
presented on both outputs rather than teleporting one plane between CRTCs.
Cursor-only motion requests presentation without dirtying the Bevy scene;
motion arriving during an in-flight flip is deferred until that flip retires.
An oversized cursor, rotated output, unsupported plane, or ineligible geometry
uses the shared composition blitter as a correctness fallback.

## Inactive and capture targets

The host has two explicit routes: `ActivePhysical` and `InactiveOwned`. It stops
queueing physical work before requesting a VT switch, pauses the Smithay output
manager when libseat reports suspension, and continues demand-driven Bevy work
against the owned target. Inactive client frame callbacks complete after a
successful owned submission.

Activation calls `DrmOutputManager::activate(true)`, clears admission for a
queued frame whose vblank can no longer arrive, and requests a fresh full
physical frame. Its callbacks already completed at submission. Repeated
notifications are harmless.

Capture always reads an owned `COPY_SRC` texture. An active capture performs a
one-shot owned composition, writes the PNG through the shared readback helper,
then requests a new direct physical composition. It never reads a scanout
allocation or introduces an owned-to-scanout blit.

## Session locking

DRM sessions expose `ext-session-lock-v1`. Run `swaylock` from a terminal or
an `exec` binding inside Weld; Swaylock owns password authentication through PAM.
For a simple manual check:

```sh
swaylock --color 1e1e2e
```

The native host gives the locker exclusive local presentation and input.
It clears each output to opaque black and composites the lock surface tree into
the same leased DRM targets, using the existing device, queue and blitter.
Lock confirmation waits for protected page flips on every output; generation
tags exclude earlier desktop frames. Lock surfaces use the shared surface-tree
importer, including the subsurface containing Swaylock's typing indicator,
buffer release, scaling and frame callbacks. The current lock composition accepts
SHM buffers with normal transforms; unsupported content retains the opaque fallback.
Protocol advertisement is enabled for DRM presenters.

Ctrl+Alt+F1–F12 remains available while locked, using Weld's configured modifiers.
Returning to the Weld TTY preserves the lock. Ordinary desktop shortcuts and
local application input remain blocked until the owning locker unlocks.
The local XKB state also drives physical Caps, Num and Scroll Lock LEDs, including
newly attached keyboards. Remote-seat modifiers leave those LEDs unchanged.

Existing hoists continue streaming and receiving their remote client's input.
Local output capture sessions are stopped and new capture requests are rejected
while locked. If the locker disconnects, the local display stays covered; a new
locker connected to the same Weld socket can take over authentication.

The isolated regression suite checks competing lockers, output confirmation,
disconnects, input isolation, continuing remote input/frame callbacks, configured
XKB remappings and capture rejection. It also launches real Swaylock against a
private two-output server and exercises repeated typing without submitting a
password. Offscreen GPU readback verifies opaque fallback and scaled/clipped
subsurface placement and removal:

```sh
cargo test -p weld-core --features test-support session_lock -- \
  --include-ignored --test-threads=1
```

Manual DRM validation with the corrected typing indicator was reported working
on 2026-10-11. For regressions, check authentication, TTY switching and return,
configured modifier swaps, keyboard LEDs, and an active phone hoist during locking.

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

## Deferred

- Dynamic output hotplug and mode changes.
- Native completion-fence export.
- Partial Bevy scene damage.
- Primary direct scanout and overlay promotion.
- `wp_presentation`, VRR, HDR, color management, rotation, and cross-GPU paths.
- EDID make/model/serial extraction; the optional `libdisplay-info` dependency
  is not enabled in the current development shell.
