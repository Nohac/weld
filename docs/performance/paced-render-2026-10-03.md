# Paced presentation baseline — 2026-10-03

The animated Steam workload showed roughly 20% of one CPU core in release Weld.
To isolate host, rendering integration and WM costs, `paced_render` drives the
production host with a separate GPU-backed EGL producer and interchangeable
presenters. The original Steam profile and this controlled workload are
different experiments.

## Matched baseline

Run `target/validation/paced-render-_ngbqbu2`: release opt-level 3, RADV 880M,
Khronos validation disabled, 2240×1400 at scale 1, 60 Hz virtual output, one
120 Hz producer, no input, no server decorations, five-second warmup and
twenty-second measurement. CPU includes process user and system time across
threads, excluding the producer and post-run capture.

| Presenter | CPU (% of one core) | CPU ms/composition | Compositions/s | Commits/s |
| --- | ---: | ---: | ---: | ---: |
| Direct wgpu surface | 4.55 | 0.760 | 59.85 | 120.05 |
| Minimal Weld–Bevy | 11.95 | 1.997 | 59.84 | 119.99 |
| Master | 12.25 | 2.047 | 59.84 | 120.02 |

All buffers were DMA-BUF; maximum observed GPU submissions in flight was one.
The roughly 1,200 superseded commits per case are expected when consuming a
120 Hz producer at 60 Hz. Captures confirmed the animated surfaces rendered.

The shared Bevy integration adds about 7.4 percentage points in this run; the
Master layer adds about 0.3. These are workload deltas, not a proof of unavoidable
engine overhead. Minimal still runs Weld's base surface, UI, input, extraction
and rendering integration. Main/render wall-time measurements cannot attribute
CPU costs to individual systems.

## Variation and controls

The matching 60 Hz producer control (`paced-render-np4u4707`) measured Master at
13.50%, with only four replacements over twenty seconds. Other runs varied:
the 1080p comparison (`paced-render-in2sk4wu`) measured 4.55%, 20.25%, and 14.75%
for surface, minimal, and Master. CPU placement/frequency were not controlled.
The minimal outlier means this benchmark does not consistently reproduce the
original Steam 20–30% workload. Repeat matched runs before claiming a saving.

Functional stress cases covered up to eight windows, 240 Hz per-window producer
cadence and 1,000 Hz pointer motion. Output remained near 60 compositions/s;
input reached the producer, captures contained all windows, and no growing GPU
queue appeared. The final input smoke run overlapped compilation and is excluded
from performance conclusions.

Next investigate the shared main-schedule, extraction and render-preparation
path with optimized symbolized CPU profiles. Keep the raw presenter as a
control. Separately validate changes against real nested/DRM presentation and
Steam; the fixture has simple opaque surfaces and does not exercise XWayland,
fractional scaling or physical scanout.
