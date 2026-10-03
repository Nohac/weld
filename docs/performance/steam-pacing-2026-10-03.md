# Steam XWayland responsiveness investigation

The reported symptoms were slow wheel scrolling, delayed hover feedback and slow
library-list updates, especially on the DRM backend. This investigation follows
the [Bevy plugin reduction](paced-render-2026-10-03.md). CPU cost and visible
responsiveness are separate measurements.

## Evidence

The optimized DRM run in `target/validation/steam-drm-pacing.log` contains 4,713
physical submissions. GPU submission waits had a median of 1.653 ms, p99 of
2.759 ms and maximum of 4.018 ms. Those waits do not explain a 100 ms stall on
their own. Physical presentation usually progressed at 16.67 ms intervals.

Steam's main surface supplied 2,189 buffers with a median interval of 32.903 ms
and p90 of 33.841 ms. A second large surface had a median of 33.329 ms. These
were DMA-BUF imports, including 2224 by 1354 buffers for the main surface.

One repeated phase relationship, at 14:58:55 UTC:

| Time within second | Event |
| --- | --- |
| 152.582 ms | Client frame callbacks complete at page flip |
| 156.462 ms | Weld queues the next composition |
| 157.706 ms | Steam's next buffer arrives, after that composition |
| 169.277 ms | The queued composition retires |
| 173.066 ms | Weld queues composition containing the new buffer |
| 185.922 ms | Its frame callbacks complete |

This is a client/compositor scheduling problem even though the compositor itself
is presenting at 60 Hz. It is not evidence that scroll events were lost or that
network fetching of library assets was slow.

Independent X11/EGL control runs used `eglSwapInterval(1)` with an animated
GPU-clear surface. Sway produced 902 frames in 15.008 seconds; nested Weld
produced 899 in 15.013 seconds before the callback change. Both reported the
Radeon 880M renderer. Unlike the unthrottled benchmark producer, this control
exercises X11 swap pacing. It does not reproduce Steam's CEF workload or DRM.

An additional virtual-pointer test sent 100 ordinary wheel steps at roughly
10 ms intervals while the X11/EGL client animated at 60 Hz. The X11 client
counted exactly 100 wheel button events on both Sway and nested Weld, separately
from pointer motion. Logs: `x11-pacing-sway-bq9aatup` and
`x11-pacing-nested-iubg0fp3` under `target/validation`. This rules out loss in
that tested nested forwarding path; it does not cover a physical high-resolution
wheel, DRM libinput ingress or CEF's interpretation of the events.

A fresh nested Steam run, `target/validation/x11-pacing-nested-x3_jkuob/run.log`,
provided 2,371 main-surface buffers: median interval 16.662 ms, p90 18.833 ms.
Its buffers were 1776 by 1104, so this is a backend contrast rather than a
matched-size performance A/B. X11 property snapshots for this run and
`x11-pacing-sway-wndjxlr_` showed Steam in Normal state, active and focused in
both. Sway additionally set the maximized state atoms.

Steam's own GPU reports from the earlier Sway and DRM sessions used the same
Radeon/ANGLE OpenGL renderer, Mesa version, enabled GPU compositing/rasterization,
one-copy tile updates and CEF launch options. There was no reported software
renderer fallback. Sway's reported display was 1792 by 1120; DRM Weld's was
2240 by 1400, about 56% more pixels. This difference matters for cost comparisons.

## Implemented correction

DRM client draw opportunities now complete after successful native queue
submission. The client can produce its next buffer while the queued frame waits
for scanout. Physical admission still waits for the matching CRTC page flip;
GPU-use leases still follow their existing completion mechanism. Failed, empty
and inactive submissions resolve their callback batches through the existing
fallback paths. Hoisted roots retain their independent callback ownership.

The native loop also flushes pending input/configure events before entering
presentation, so clients can react while GPU completion or swapchain acquisition
is pending. The loop-tail flush remains for callbacks produced by presentation.

The pinned Smithay Anvil implementation uses this callback phase:
`vendor/smithay/anvil/src/udev.rs` calls `post_repaint` after `render_surface`
queues its frame. Its vblank handler also explains the separate, future option
of delaying composition within the refresh interval to give clients more time.
This change does not add that repaint-delay heuristic.

## Prior reports and remaining limits

- Valve's [slow smooth scrolling report](https://github.com/ValveSoftware/steam-for-linux/issues/9219)
  describes slow scrolling and excessive CPU use, with discussion of older
  Chromium behavior. Its age and different build prevent treating it as our
  root cause.
- Valve's [laggy interface report](https://github.com/ValveSoftware/steam-for-linux/issues/8890)
  reports low UI frame rate and delayed input feedback.
- Sway's [laggy floating Steam windows report](https://github.com/swaywm/sway/issues/8710)
  specifically concerns dragging Steam settings/properties windows. It supports
  checking X11 window state, but is not a demonstrated explanation of Weld's
  wheel behavior.

These reports are leads, not fixes to transplant. The examined Weld input path
forwards wheel distance and v120 information to Smithay without video-frame
coalescing. The follow-up below distinguishes delivery from interpretation.

Verification: 142 core tests passed, nine hardware tests remained ignored;
core library/test Clippy passed and the release build succeeded. Tests cover
submission-time callback completion with physical admission still held, and
multi-output callback completion. Fable approved the final lifecycle change.

For physical DRM comparisons, measure **client buffer import intervals**,
callback-to-import phase and perceived scrolling, not just physical submission
rate. Run from the Weld TTY:

```sh
RUST_LOG=warn,weld_drm_pacing=trace,weld_surface_diag=trace \
  cargo run --release --locked -- --backend drm \
  --config examples/master.sway.config --xwayland
```

## Physical retest and wheel diagnosis

The user's 16:01 UTC DRM retest supplied 2,806 main-window buffers at 2224 by
1354, with a median interval of 16.665 ms and p90 of 17.210 ms. 99% were within
13–21 ms. At 1108 by 1354, 3,708 buffers had a median interval of 16.677 ms.
The user reported much better hover feedback, but hesitant, slow wheel
scrolling persisted while touchpad scrolling felt good. GPU wait p99 was
2.239 ms, with a 3.682 ms maximum.

`target/validation/steam-scroll.log` subsequently recorded 990 wheel events
arriving at Smithay, all with distance ±15 and value120 ±120. SwayFX 0.6,
wlroots 0.20.2 and Weld use the same libinput values; the user's Sway scroll
factor was 1.0. Increasing Weld's output scale did not resolve the issue.

The XInput2 probe found a separate input bug: Weld forwarded absolute pointer
positions but lacked relative-pointer protocol delivery. XWayland 24.1.13 sends
those movements through `xwayland-pointer`, while wheel events use
`xwayland-relative-pointer`. Alternating movement and scrolling generates
XInput2 `SlaveSwitch` events. Sway supplies relative motion alongside position,
keeping both on the same virtual device.

[Chromium's X11 event source](https://github.com/chromium/chromium/blob/main/ui/events/platform/x11/x11_event_source.cc)
invalidates a device's scroll baseline on a slave switch.
[Its scroll-offset calculation](https://github.com/chromium/chromium/blob/main/ui/events/devices/x11/device_data_manager_x11.cc)
then consumes the first valuator sample as a baseline with zero scroll offset.
This explains why ordinary X11 button-count tests miss the failure.

The bounded motion-plus-wheel probe in `x11-pacing-nested-h0xtyqkd` delivered
all 100 notches, but produced 201 slave switches; applying Chromium's baseline
logic ignored all 100 scroll samples. The corresponding Sway run,
`x11-pacing-sway-d8ct8h6r`, had zero switches and only its first sample established
a baseline. Additional physical input occurred during the Sway run, so its
144 samples are not a matched event-count comparison.

The correction now preserves relative motion through the shared input path and
delivers it with position in one Wayland pointer frame. Iroh and application
coalescing accumulate deltas. In the post-fix nested probe
`x11-pacing-nested-ikizak7i`, only the initial baseline was ignored, with no
repeated device switching during scrolling. Its 94 received samples are not
claimed as a lossless 100-event run; the input interpretation failure is the
measured correction. The user confirmed that physical DRM Steam scrolling now
works correctly, including slow wheel notches, and that the themed X11 default
cursor looks consistent with Weld chrome.
