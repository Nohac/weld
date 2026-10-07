# Probe-local AnyRender patch

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
