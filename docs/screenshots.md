# Screenshots

Weld's nested and DRM presenters expose output capture through
`ext-output-image-capture-source-v1` and `ext-image-copy-capture-v1`.
Grim 1.5 supports these protocols. Slurp supplies region selection through
Weld's layer-shell support, and `wl-copy` owns clipboard delivery.

From a terminal launched inside Weld:

```sh
grim screenshot.png
grim -c screenshot-with-cursor.png
grimshot save area screenshot.png
grimshot copy area
grimshot copy screen
grimshot copy active
grimshot copy window
grimshot copy output
```

Grim handles output stitching, region cropping and requested image scaling.
Weld advertises each output's physical buffer size and captures the complete
composition, including shell decorations and layer surfaces. The capture request
selects whether the local cursor is included.

Clipboard utilities use `ext-data-control-v1` or `wlr-data-control-v1` to transfer
the image directly through the named logical seat. This preserves focus and
layout: without data-control, `wl-copy` and `wl-paste` acquire focus through a
temporary `wl-clipboard` toplevel, which caused a brief tiled-layout rearrangement.
Clients admitted to the local Wayland socket can read and replace clipboard
selections through these protocols.

Grimshot's `active`, `window`, `output` and `anything` selectors use Weld's
Sway IPC tree/output queries. Child applications inherit `SWAYSOCK` and `I3SOCK`.
The tree includes nested tiling groups, floating windows, global logical geometry,
selection, visibility and application metadata. Vacant persistent slots remain
in the tree and are excluded from Grimshot's application selection boxes.

## Ownership and safety

Local clients with access to Weld's Wayland socket can request captures. Portal
permission prompts and browser/OBS PipeWire sharing are separate follow-up work.
An active session lock stops output capture sessions and rejects new captures;
capture tools can start a fresh session after unlock.

Smithay owns protocol resources and validates capture buffers. Weld bounds
active sessions and pending frames, cancels disconnected requests, refreshes
constraints after output changes and fails requests after ten seconds.
Presentation-free hosts reject capture sessions until a local output presenter
is available.

The GPU copies a completed composition into a dedicated readback buffer on the
composition queue before that texture is reused. One bounded worker waits for
GPU completion and maps the copied pixels. An eventfd wakes the host to deliver
pixels into validated client SHM and send `ready`. Cursor state is sampled when
the copy is submitted. Startup and development-debug PNG captures share this
worker; PNG encoding and filesystem writes also run there.

## Validation

Run from the normal Rust development environment on an existing desktop:

```sh
scripts/check-screenshot-capture
cargo test -p weldwm -p weld-core --lib \
  async_capture_keeps_each_snapshot -- --ignored
```

The script launches an isolated nested Weld and Foot, checks full/region PNGs,
cursor capture, concurrent clients, cancellation, live IPC geometry,
Grimshot active/output saving, clipboard byte equality and clipboard transfer
without temporary toplevels. Logs and PNGs remain
under `target/validation/screenshot-*`.
The Vulkan regression overwrites a source texture between two queued captures
and verifies every pixel of both asynchronous snapshots.

On DRM, manually check region selection and cursor placement at your configured
fractional scale, including a selection near each output edge. Multi-monitor
capture also needs a physical test with differently scaled outputs.
