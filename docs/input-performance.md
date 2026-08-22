# Input performance findings

This note preserves conclusions from Weld's removed low-level DRM presenter
and the subsequent Smithay-first replacement. Historical measurements remain
useful when they explain constraints retained by the current backend.

## Required behavior

- Focused Wayland clients receive ordered raw input at device-event pace.
- Bevy receives the same input in refresh-paced batches so policy, picking,
  hover, and Leafwing state do not run at mouse polling rate.
- Pointer presentation must not require a complete Bevy scene composition.
- Held interactions remain active until their terminating button release,
  independent of output changes or later hit tests.
- Input conversion should reuse bounded storage and avoid per-event allocation.

## Measurements

Headless benchmarks showed that raw batch ingress and ordinary Bevy schedules
were small compared with live physical presentation. Synthetic bursts of 16
pointer events changed the measured application path by well under one
microsecond per frame on the tested system. A mapped client added more fixed
render work than the input burst itself.

Whole-process traces then showed that the old compositor coupled pointer motion
to physical scene presentation and duplicated cursor scheduling around its
custom KMS state. Moving the pointer could drive Weld from an idle floor to
roughly 10 to 12 percent CPU. Moving cursor pixels to a Smithay-backed hardware
cursor plane reduced rapid-motion CPU to about 3.5 percent on the same machine,
which demonstrated that cursor presentation, not Bevy event projection alone,
was the dominant avoidable cost.

Input allocation cleanup remains useful for a long-running compositor but did
not materially change CPU utilization. The old adapter used reusable event
batches and coalesced only application-facing pointer motion while forwarding
client events in order. That split should remain; the removed physical-frame
scheduler should not.

Rapid external-mouse motion produced substantially more libinput wakes than a
touchpad, but lower average work per wake. Event batching and device shape make
single per-wake averages misleading. Compare complete process CPU and kernel
time in addition to Tracy zones before attributing a regression to conversion
code.

## Rebuild guidance

The Smithay-first backend should begin with these hypotheses rather than
recreating the old instrumentation:

1. Let Smithay own libseat, libinput calloop sources, output state, and hardware
   cursor plane submission where its public APIs already model them.
2. Forward raw events to the focused client immediately, while retaining a
   bounded batch for the next application update.
3. Pace application updates and scene composition from output demand, not from
   input-device polling frequency.
4. Model cursor image and position as retained state; a new motion event should
   replace the desired position rather than allocate presentation work.
5. Keep hardware cursor failure explicit and capability-driven. GPU fallback
   should damage only cursor bounds once partial composition is available.
6. Re-run the headless benchmarks first, then collect whole-process profiles on
   the new adapter before adding special scheduling policy.

The old DRM-specific profiling suites were removed with their backend. Generic
Tracy, render benchmarks, and whole-process profiling remain documented in
[Profiling Weld](profiling.md).

## Smithay-first validation

The first production trace on the replacement backend exposed a free-running
composition timer drifting against physical vblank. Of 4,195 measured
retirement intervals, 738 skipped exactly one refresh, including sustained
33.3 ms runs on the 60 Hz panel. Frames admitted more than roughly 11 to 12 ms
after vblank routinely missed the next flip even though blocking GPU waits
averaged only 1.38 ms.

The physical backend now uses Smithay's retired DRM frame as its pacing clock.
In the follow-up trace, 3,194 of 3,262 measured retirements completed on the
next refresh and 3,176 submissions were admitted within 1 ms of vblank. The
previous sustained 30 Hz state did not recur, and Smithay assigned every
recorded cursor update to the hardware cursor plane. Remaining isolated gaps
coincided with application or trace-output stalls rather than a late periodic
deadline.

On the tested AMD Radeon 880M system, an uninstrumented release build stabilized
at roughly 3.5 to 4 percent Weld CPU during rapid pointer motion and roughly 10
to 11 percent while Firefox played two 4K60 YouTube videos. These figures are a
useful baseline, not a performance guarantee; native completion fences, partial
scene damage, and further whole-process profiling remain future improvements.
