# Remaining OBS capture CPU and source pacing — 2026-11-08

## Live evidence after the first fixes

The user reported approximately 45% → 35% CPU after installing `2b643cc`.
The updated session used Weld PID 216917 and OBS PID 217499. OBS and htop
were visible; the captured Brave window was on another workspace.

A 15-second userspace profile collected 1,558 samples at 499 Hz, approximately
20.8% of one core. `--user-callchains --call-graph dwarf,16384` allowed this
userspace-only attachment without changing kernel permissions. Samply recovered
the stacks, as in the earlier investigation. Approximate core percentage points:

| Recognizable caller/thread | Core points |
| --- | ---: |
| Window capture preparation/submission | 4.48 |
| Desktop render/extraction/cache checks | 3.91 |
| Capture completion worker | 1.74 |
| Bevy main/policy update | 1.36 |
| Host SHM snapshot | 1.32 |
| SHM staging upload | 0.53 |
| Remaining dispatch/presentation/completion and unattributed stacks | 7.47 |

The former second SHM pixel-vector copy is absent. A separate ten-second
process sample measured approximately 22.3% user CPU and 7.7% kernel CPU.
These are live observations, not a controlled before/after comparison; core
placement and workload timing can change utilization.

The user supplied a kernel-only capture: 824 samples, zero lost samples,
approximately 1.651 CPU-seconds in 20 seconds (8.26 core points). GPU submission
stacks include `amdgpu_cs_ioctl`, buffer validation, reservation fences, memory
allocation, and lock contention. Event-loop polling, Wayland socket operations,
and completion wakeups also contribute. Kernel CPU is not Bevy layout time.

Artifacts are under `target/validation/`: `obs-after-live.data`,
`obs-after-samply.json`, `obs-after-analysis.txt`, `obs-after-thread-cpu.json`,
`obs-after-kernel-report.txt`, and `obs-after-pipewire.json`.

## Empty render-finalization submission

Bevy 0.20's `render_system` unconditionally created, finished, and submitted
an encoder for screenshots/readbacks after its render graph, even when both
request collections were empty. The vendored patch now checks those collections
before creating the encoder. Screenshot collection and presentation still run.
This eliminates an ordinary per-frame queue submission, independently of whether
client pixels arrive through SHM or DMA-BUF.

The application regression requests a Bevy manual-target screenshot and GPU
texture readback after idle frames, waits for both completion events, and checks
their pixels. All 100 application tests passed on the GPU.

An optimized offscreen ABBA comparison used the existing 900×600 SHM-update
fixture, CPU 4, 30 warmup frames and 1,200 measured frames. Both versions perform
zero main updates during measurement. This isolates render-finalization work
on top of the previous SHM fix, not the whole live desktop.

| Run | CPU µs/frame | Wall µs/frame |
| --- | ---: | ---: |
| Before | 491.67 | 1,574.83 |
| After | 441.67 | 1,501.22 |
| After | 425.00 | 1,541.09 |
| Before | 475.00 | 1,596.63 |

Mean CPU falls approximately 10%. Executables and logs are retained as
`empty-submit-before`, `empty-submit-after`, and `empty-submit-abba-*.log`.

## Recorder demand was driving duplicate window copies

The live PipeWire video format was BGRA DMA-BUF, 2240×1358, with variable
framerate `0/1` and no negotiated maximum in the reported format. A bounded
`pw-profiler -J` observation recorded 1,205 distinct driver cycles for portal
node 121 over 2.973 seconds: approximately **405 cycles/second**. This is a
graph scheduling measurement, not a direct count of completed GPU copies.
The JSON is retained in `obs-pipewire-profiler.log`.

The portal's ext-image-copy ready handler queues the buffer and triggers
PipeWire processing; processing requests another capture. Weld previously
served every request from the current retained scene, regardless of whether
the source had changed. The 60 Hz hidden-client presentation claim paced the
client's callbacks, but did not pace these repeated capture requests.

Window capture now compares each session's last successfully delivered content
revision and cursor state. A first frame is guaranteed; later requests wait
until a root/subsurface commit, popup lifecycle change, or included-cursor change
invalidates that content. Waiting for unchanged content does not consume the
ten-second transfer deadline. Once copying starts, the transfer deadline is
renewed. Completion records the delivered revision only on success. A cancelled
request still withdraws its presentation claim, and existing GPU lifetime rules
continue to protect submitted work.

This follows ext-image-copy-capture's `capture` request contract, which permits
an indefinite wait for source changes after a session's first successful frame.
The change applies to individual-window streams; whole-output capture retains
its existing cached-frame delivery policy.

The new real-Wayland idle regression fails against installed `2b643cc` with
`unchanged window produced another frame` (`capture-idle-before-mq6bsonf`).
With the fix it keeps a request pending for 11 seconds, then completes after
the client repaints. The compositor submits exactly two copies: initial content
and the repaint. This also verifies an idle wait longer than the previous
transfer timeout.

The first full headless run (`output-capture-w_53phhk`) passed DMA-BUF/SHM pixel
equality, resize/close, XWayland/GTK, cursor/cancellation, independent snapshots,
the idle/resume regression, and portal recording of an animated hidden window.
That fixture changes at 5 Hz: its capture trace contains 22 copies over about
3.90 seconds, while Kooha produces a decodable recording with continuing motion.
Static snapshot probes use fresh sessions to exercise the first-frame guarantee;
they are explicitly distinct from a continuous stream's change-driven delivery.

The final headless run (`output-capture-rwcpg2bg`) additionally requests 240 fps
from a hidden terminal changing at 60 Hz. It receives 120 frames in 2.118 seconds
(including probe setup), with zero additional desktop compositions. Starting
capture lets a hidden client acknowledge its pending startup resize; the fixture
reopens with updated buffer constraints before timing steady delivery. The
complete portal/Kooha, idle/resume, cursor, resize/close and pixel tests pass.
Seven focused core capture tests pass; five opt-in tests are excluded by that
command. Clippy completes with warnings in unchanged code, and formatting checks
pass. These measurements establish the eliminated duplicate-copy behavior;
the new live OBS CPU percentage requires restarting into this build.
