# OBS window capture CPU investigation — 2026-11-08

## Workload and measurement

Installed release `b13fdd61aa3471f1d84fdc07b8646dde198baac2`, DRM backend,
Weld PID 166134, OBS 32.2.2 PID 174993. OBS captures a browser on another
workspace, with both configured for 60 fps. PipeWire negotiation shows a
2240×1358 BGRA DMA-BUF stream, modifier 144115188348492548. Destination
DMA-BUF negotiation does not identify the buffer type of OBS's own window.

Two user-supplied 20-second `task-clock:u` recordings used 499 Hz sampling
and 16 KiB DWARF stacks. The first contains 2,035 samples, approximately
4.08 CPU-seconds; the second contains 4,533, approximately 9.08 CPU-seconds.
This is approximately 20.4% and 45.4% of one core in userspace respectively,
not total CPU including kernel execution. The user confirmed OBS's preview
was visible during the second recording. Window inventory near the first
recording showed a terminal visible and OBS/browser hidden. These are live
observations, not a controlled A/B experiment or a measurement of actual
delivered frame count.

Perf 7.3's libdw unwinding lost most Rust callchains: 2,803 samples in the
second recording produced no stack frames. Flat instruction-pointer samples
remained valid. Importing the same file with Samply 0.13.1 recovered stacks
for all 4,533 samples; only two were single-frame. Addresses were resolved
against the installed executable and mapped libraries' symbol tables.
Some stacks still end before the outer caller because the captured stack is
bounded. Do not use the failed libdw inclusive report for attribution.

Local artifacts under `target/validation/`:

- `obs-browser-live.data`, `obs-browser-better.data`: original recordings.
- `obs-browser-first-samply.json`, `obs-browser-samply.json`: recovered profiles.
- `analyze-samply.py`, corresponding `.stacks.json` files and `*-analysis.txt`:
  symbol resolution and inclusive/self summaries.
- `obs-browser-better-leaf.txt`: independent perf self-sample report.
- `capture-live-pipewire.json`: negotiated stream properties.
- `obs-browser-registers.txt`, `obs-shm-ownership.json`: SHM attribution evidence.

## Where the CPU goes

The following assigns each sample to one category, using recognizable caller
frames and thread identity. Values are approximate percentage points of one
core (`samples / 499 / 20 × 100`), not percentages within an inclusive tree.
Unattributed stacks remain explicit. Classification is conservative: truncated
ancestors can leave some work outside its actual high-level owner.

| Work | First recording | OBS preview visible |
| --- | ---: | ---: |
| Desktop render, extraction and retained-UI checks | 2.62 | 10.07 |
| Bevy main/policy update | 0.76 | 8.07 |
| Host SHM snapshot and normalization | 0.05 | 6.49 |
| Second SHM copy at ECS ingress | 0.03 | 2.77 |
| Capture preparation and GPU submission | 6.85 | 5.42 |
| Capture GPU completion worker | 2.94 | 2.01 |
| Capture completion service on main thread | 0.63 | 0.93 |
| Other Wayland dispatch | 1.61 | 2.81 |
| Client adapter delivery | 0.90 | 1.30 |
| Other native presentation | 1.25 | 1.74 |
| Other presentation preparation | 0.19 | 0.37 |
| DMA-BUF completion worker | 0.06 | 0.41 |
| Iroh worker | 0.05 | 0.05 |
| Other/unattributed | 2.44 | 2.98 |

Within the second recording, UI layout accounts for about 0.70 points, and
Bevy image preparation/upload for about 2.54 points. These are subsets of
the table, not additional costs. No recovered sample contains Leafwing;
this provides no evidence that input handling is a leading cause here.

## Which buffers use SHM

OBS and Weld map the same `memfd:wayland-shm` allocations. In particular,
inodes 77795 and 101467 each occupy 0x6d2000 bytes, about 6.8 MiB. Of the
885 samples whose leaf is `memmove`, 228 have an RSI source address inside
those two OBS-owned mappings in Weld: 176 in inode 77795 and 52 in inode
101467. For example, sample time 110624.554550 has RSI 0x7252dc81bb2c,
inside Weld's mapping 0x7252dc5a9000–0x7252dcc7b000 of inode 77795.
This directly identifies OBS-owned SHM as a source of the copying, rather
than inferring ownership solely from a function name. It does not attribute
every copy to OBS or prove which individual Qt widget caused a commit.

OBS's capture input, its GPU preview, and its Qt interface are separate
paths. Qt documents that most QWidget applications use shared-memory
buffers ([Qt Wayland README](https://github.com/qt/qtwayland/blob/dev/README)).
OBS's [32.2.2 display widget](https://github.com/obsproject/obs-studio/blob/32.2.2/frontend/widgets/OBSQTDisplay.cpp)
creates a native window and passes its Wayland surface to the OBS display
renderer. A GPU-rendered preview can therefore coexist with SHM interface
buffers. Finding SHM does not establish a software-rendered video preview
or a portal DMA-BUF fallback.

## Confirmed work amplification in Weld

The SHM ingress chain is:

1. `server/shm.rs::copy_shm_contents` snapshots the client buffer and
   normalizes XRGB alpha. It copies the complete image, not just damage.
2. `AppShell::prepare_surface_commit` attempts to take sole ownership of
   that snapshot. When other owners retain it, `bgra_pixels.clone()` copies
   it again. Recovered stacks attribute approximately 2.77 core points to
   this ingress branch, overwhelmingly in `memmove`.
3. `surface.rs::apply_surface_tree_snapshot` unconditionally treats
   `SurfaceBufferContent::Pixels` as `policy_changed = true`, including
   same-size updates with unchanged scene geometry. This requests the full
   Bevy main schedule.
4. The same path modifies an `Assets<Image>` asset and increments its
   generation. Image asset changes invalidate retained UI preparation;
   Bevy prepares/uploads the image again, and bindings must follow the new
   GPU image generation.

This explains why stable geometry does not currently mean a retained frame
for SHM clients. Removing the policy flag alone is insufficient: asset event
delivery, texture upload, image identity and binding lifetime must all remain
correct without a main update.

The capture-only path has a different cost. `WindowImageCapture::submit`
uses the shared `SourceCompositor`, bypassing Bevy layout. For DMA-BUF
sources and destination it creates separate source-acquire, destination-acquire,
draw, destination-release and source-release command buffers, submitted
together. The recovered stacks show allocation, command encoding/submission,
and command-pool reset in completion handling. In the first recording,
`vk_common_ResetCommandPool` alone appears in 7.22% of inclusive samples,
about 1.47 core points; it is already included in the table above.
`SourceCompositor::prepare` also rewrites geometry uniforms every frame,
even when placement is unchanged. These are CPU costs of preparing GPU
work, not CPU pixel readback.

## Optimization boundaries established by this evidence

For SHM content, maintain a stable uploaded texture and bindings while size,
format, opacity and geometry are unchanged. Upload from the retained snapshot
without cloning it into a second CPU-owned pixel vector. Separate pixel
delivery from policy/layout and general image-asset invalidation. First
mapping, resize, buffer-kind transitions, opacity changes, unmap, and teardown
must still invalidate the appropriate state. Damage-aware snapshots/uploads
are a separate opportunity requiring correct retained-image semantics.

For capture, investigate combining command recording while preserving every
foreign queue ownership barrier and source/destination completion lifetime.
Cache unchanged geometry uniforms. Measure this independently of visible
desktop rendering; suppressing Bevy work cannot remove the capture worker's
command-pool reset cost.

No performance fix or reduction is claimed by this report. Validation of
changes should cover repeated same-size SHM and DMA-BUF updates, visible
and hidden capture, pixel correctness against full preparation, resize and
buffer-kind transitions, resource retirement, and equivalent optimized
headless workloads before repeating the live OBS measurement.
