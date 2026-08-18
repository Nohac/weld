# Multi-output validation

**Status: Observed issues and agreed follow-up.** This note records behavior
seen during development on a mixed-scale two-output DRM setup. Observations are
kept separate from possible causes until profiling or instrumentation verifies
them.

## Scale-boundary movement

Dragging a window between outputs with different scales can briefly flash or
jump as it crosses the boundary. The transition can change the window's
preferred client scale and its output projections, but the responsible stage
has not been isolated. In particular, do not describe this as output-texture
resizing without evidence: moving a window does not resize the output
composition targets.

Investigation should correlate one crossing with:

- `WindowOutput`, `WindowOutputIntersections`, and `WindowPreferredOutput`
  changes;
- projection creation and retirement on both output cameras;
- preferred fractional-scale protocol updates and subsequent client commits;
- DMA-BUF identity, import reuse, and material binding changes; and
- composition and presentation timing for both outputs.

The result should distinguish a geometry jump, one stale or duplicated
projection frame, a client buffer replacement, and a missed presentation
deadline rather than treating all four as texture churn.

## Cross-boundary interactive resize

Interactive resizing while a window overlaps two outputs has been observed to
raise CPU usage to roughly 50 percent. This needs a focused trace which records
the Weld process and the client separately. A DMA-BUF client surface is sampled
directly during normal composition, so high CPU usage alone is not evidence of
a CPU pixel copy. Resizing can still cause frequent client buffer allocation,
DMA-BUF import and retirement, bind-group preparation, projection work, and
full composition on both outputs. A software-rendered or SHM client has a
different copy path and must be identified explicitly in the report.

The first profiling pass should record buffer type, new allocation identities
per resize step, import/cache hit rate, Bevy main and render schedule time, GPU
completion waits, composition count per output, and CPU usage of both Weld and
the resized client. Any hidden readback or pixel copy is a defect; do not infer
one solely from utilization.

## Pointer topology while scale is uncalibrated

Physical millimeter footprints are useful only when output placement and scale
describe the intended physical relationship. With independently chosen output
scales, the physical portal layout can feel inconsistent with the logical
desktop in which windows and applications are manipulated.

The agreed short-term policy is to use logical output rectangles for pointer
collision, edge sliding, and portals. The implementation still uses physical
footprints at the time of writing; changing it is a separate slice. Preserve
measured physical metadata, topology diagnostics, and the physical scale-match
shortcut so a later calibrated physical-layout mode remains possible. Output
configuration should eventually make logical and physically calibrated pointer
topologies explicit choices rather than deriving an unexpected hybrid.
