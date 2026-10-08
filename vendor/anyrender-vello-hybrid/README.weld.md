# Weld native-client AnyRender patch

Source: `anyrender_vello_hybrid` 0.11.0 from crates.io, upstream AnyRender commit
`cf0f14776102ec775af61bcdcaac2ab345f4fafb`, directory
`crates/anyrender_vello_hybrid`. The Rust sources and normalized package manifest
are retained with the upstream MIT and Apache-2.0 licenses.

The local change in `window_renderer.rs` supplies the window's owned display
handle when constructing the first Linux WGPU instance. WGPU 30's GLES backend
otherwise selects a surfaceless EGL platform because `wgpu_context` 0.10.0
initializes with `display: None`. A Wayland window then fails adapter selection
with `incompatible_surface_backends: Backends(GL)`.

The small `NativeDisplay` wrapper adds the Debug bound required by WGPU while
retaining the actual window/display owner. Existing devices are reused on
resume. Android initialization is unchanged.

Remove this patch when the upstream renderer/context passes the native display
before adapter selection. Revalidate Linux Wayland startup and suspend/resume.

Upstream: https://github.com/DioxusLabs/anyrender/tree/cf0f14776102ec775af61bcdcaac2ab345f4fafb

## Completion-pacing experiment

`probe_timing.rs` records per-second wall-time totals for the six render phases.
The live probe calls `set_probe_nonblocking_poll` before launching the renderer;
the default uses nonblocking completion polling. `--blocking-poll` restores the
upstream wait for comparisons. The change replaces only the final
`Device::poll(wait_indefinitely())` with `PollType::Poll`. WGPU
continues owning resource tracking and completion callbacks, and the importers
retain their image/fence contracts. This allows CPU work for a subsequent frame
to overlap outstanding GPU work.

The Pixel's measured completion wait fell from about 7.95 ms to 0.034 ms per
frame, restoring approximately 60 selected frames/s. The probe and Weld Connect
share this patch. See the [probe README](../../tools/dioxus-texture-probe/README.md)
for matched queue-policy comparisons and validation boundaries. Remove these
diagnostic controls when an upstream asynchronous completion policy replaces
the per-frame blocking wait and equivalent measurements are available.
