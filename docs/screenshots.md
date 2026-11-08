# Screenshots

Weld's nested and DRM presenters expose output capture through
`ext-output-image-capture-source-v1` and `ext-image-copy-capture-v1`.
Native Wayland and XWayland toplevels are listed through
`ext-foreign-toplevel-list-v1` and selected through
`ext-foreign-toplevel-image-capture-source-v1`.
Capture accepts both SHM and compatible DMA-BUF destinations. The latter can
feed screen recorders through `xdg-desktop-portal-wlr` and PipeWire without CPU
pixel readback in Weld. Both whole-output and individual-window sources are
implemented. Workspace sources and a native OBS plugin are not implemented yet.
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

Ordinary clipboard-reader connections discover the desktop seat. Remote seats
are advertised only to the application connections receiving their input, and
retain separate clipboard contents through controller handoffs. This also keeps
unspecified-seat readers such as Arboard/wl-clipboard-rs from randomly selecting
an unrelated remote clipboard. Local socket access remains trusted; explicit
clipboard-sharing permissions and restricted helper connections are future work.

Grimshot's `active`, `window`, `output` and `anything` selectors use Weld's
Sway IPC tree/output queries. Child applications inherit `SWAYSOCK` and `I3SOCK`.
The tree includes nested tiling groups, floating windows, global logical geometry,
selection, visibility and application metadata. Vacant persistent slots remain
in the tree and are excluded from Grimshot's application selection boxes.

## Individual windows

Window capture uses the application's rectangular viewport and its popup /
subsurface composition, shared with phone streaming. Weld titlebars, borders,
rounded clipping, and other desktop windows are excluded. Application-drawn
chrome remains part of the client's pixels. Like the existing streaming view,
the capture output is opaque and popups are constrained to the viewport.

The recorder supplies its destination buffer. Weld draws directly into that
DMA-BUF on the GPU; it does not hand the recorder an application's reusable
buffer or read pixels back through the CPU. SHM remains available for consumers
that require it. Source buffer leases survive GPU completion even if a capture
request is cancelled. Destination buffers are released only after their GPU
writes finish. Imports and in-flight work are bounded.

Active frame requests add an independent presentation claim to the selected
window and its popups. This keeps a captured window updating on an inactive
workspace without composing the whole desktop. An open session with no frame
requests adds no rendering demand. Visible native windows keep their normal
frame callbacks and presentation feedback; capture only supplies the missing
demand for hidden windows. Closing, unmapping, locking the session, or
ending capture withdraws that claim without removing another stream's claim.
The window-source protocol requires an available DRM render node.

Each window-capture session receives its first frame promptly. Subsequent frames
wait for changed application content, popup state, or an included cursor change.
An idle stream remains valid while waiting; repeated recorder requests do not
cause repeated GPU copies of the same scene. New independent snapshot sessions
retain their first-frame guarantee.
Window resize updates buffer constraints; old source handles remain stopped
if the application later remaps.

OBS can use its PipeWire window source. Kooha 2.3.1 gates window requests behind
an experimental flag:

```sh
KOOHA_EXPERIMENTAL=window-recording kooha
```

Use the portal's source picker to choose a window. The portal backend must use
a window-capable picker such as Rofi's dmenu mode; `chooser_type=none` always
selects an output. The default portal-wlr chooser skips Slurp when windows
are requested and tries its menu pickers. This does not add workspace sources.

## Ownership and safety

Local clients with access to Weld's Wayland socket can request captures. The
external desktop portal backend owns its picker and permission flow; configure
the ScreenCast portal to use a version of `xdg-desktop-portal-wlr` supporting
`ext-image-copy-capture-v1`. The installed 0.8.4 backend was verified through a
private portal session and PipeWire consumer. Weld does not encode or store
these streams; the recorder does that.
An active session lock stops output capture sessions and rejects new captures;
capture tools can start a fresh session after unlock.

Smithay owns protocol resources and validates capture buffers. Weld bounds
active sessions and pending frames, cancels disconnected requests, refreshes
constraints after output changes and fails requests after ten seconds.
Presentation-free hosts reject capture sessions until a local output presenter
is available.

For DMA-BUF capture, capability discovery advertises only single-plane ARGB/XRGB
formats and explicit modifiers supported for rendering and protocol import.
The host validates dimensions and flags and waits asynchronously for the
destination's implicit reader/writer fences before acquiring it. A GPU blit
copies the completed composition, optionally paints the cursor, and returns
foreign ownership. A separate bounded completion worker retains the import and
allocation until the GPU finishes; only then does the host send `ready`.
Disconnect and cancellation do not free submitted GPU storage early. Outstanding
GPU jobs and cached imports are bounded independently of request cancellation.

DRM capture takes a GPU snapshot after Bevy composition and before cursor-plane
fallback and foreign release. This snapshot is independent of scanout allocation
reuse. It exists only while the output has capture sessions. Nested capture uses
its retained owned texture. Repeated capture requests reuse the latest content
without running Bevy again; cursor changes can still appear in captured frames.
Capture retains no CPU image copy on the DMA-BUF path. Recorder-side conversion
and encoding may introduce their own copies.

For SHM capture, the GPU copies a completed composition into a readback buffer on the
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
cargo test -p weldwm -p weld-core --lib \
  clipboard_readers_keep_desktop_selection -- --ignored --test-threads=1
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

## Recording and DMA-BUF validation

```sh
scripts/check-output-capture
scripts/check-output-capture --portal
scripts/check-output-capture --kooha
cargo test -p weldwm -p weld-core --lib capture_snapshot_survives -- --ignored
cargo test -p weldwm -p weld-core --lib dmabuf_capture_preserves -- --ignored
```

The capture runner defaults to a private GPU-backed headless Weston parent;
Weld still uses its nested presenter inside that parent. Install Weston or set
`WELD_TEST_WESTON` to its executable. If the parent cannot start, a visible nested
fallback is allowed on another identified desktop. On Weld (or an unidentified
desktop), the runner stops instead. `--visible` explicitly selects desktop tests.
Weld's presentation-free `--backend headless` cannot test output capture.

The optional portal check also needs PyGObject with GIO, Gst and GstAllocators
typelibs and the GStreamer PipeWire plugin. Set `WELD_TEST_PORTAL_FRONTEND` and
`WELD_TEST_PORTAL_BACKEND` to the portal executables; `WELD_TEST_PORTAL_PYTHON`
can select a Python interpreter with those modules. It runs private portal
services on a private D-Bus bus, selecting only the isolated `weld-nested` output.
The existing PipeWire server transports the test stream. Live portal services
and their settings remain untouched.
`--kooha` additionally starts the installed application with private settings,
records through its normal window action, and checks the resulting WebM with
FFprobe. `--kooha-region` instead exercises the region preview, drags a selection,
presses Done, and verifies that preview shutdown and cropped recording complete.
`--kooha-window` records an animated window on an inactive workspace, verifies
that the portal returns a window source, and also compares capture pixels for
a GPU-backed GTK window. The base runner checks same-session window resizing
and closes a fixture to verify capture termination and foreign-handle cleanup.
All recorder modes check motion after the first two seconds, and save a CPU
sample. The recorder uses its packaged media libraries instead of inheriting the
development shell's GStreamer plugin/library paths. Recordings stay in the
validation directory.

Verified on 2026-11-08 with the AMD render node, headless Weston 16, portal-wlr
0.8.4, portal 1.22.1, and PipeWire 1.6.9:

- Exact SHM/DMA-BUF pixel equality for a static output and repeated buffer reuse.
- 30/60 fps capture, cursor inclusion, and recovery after a disconnected client.
- Zero additional scene compositions during each three-second static capture.
- Portal CreateSession/SelectSources/Start/OpenPipeWireRemote followed by 90
  frames whose GStreamer memory objects were all DMA-BUFs. The probe explicitly
  negotiates linear XR24 and uses an eight-buffer pool; an unconstrained test
  consumer and a two-buffer pool did not provide a reliable streaming test.
- A GPU regression preserves captured pixels after the display texture is
  overwritten and verifies reuse of the retained snapshot allocation.
- A render-node regression exercises foreign acquire/render/snapshot/release
  with real GBM allocations, reuses the capture import, and checks its pixels.
  This passes with Khronos Vulkan synchronization validation enabled. Validation
  also exposed an array-layout issue in the shared fullscreen blit shader;
  deriving the triangle vertices arithmetically removes it.
- Installed Kooha 2.3.1 recorded a four-second 960×640 WebM with 121 decoded
  frames. An animated terminal and decoded frame hashes after the first two
  seconds verify motion, not merely repeated frames. This passed with both the
  installed portal-wlr 0.8.4 and an isolated upstream 0.8.5 build; no backend
  upgrade was needed for that whole-output test. The region workflow has
  additional failures described below. Kooha's default software VP8 profile negotiated SHM.
  DMA-BUF delivery is separately verified by the portal consumer above.

Logs and measurements are written under `target/validation/output-capture-*`.
The short static 960×640 debug-build samples used 0.12 CPU-seconds at 30 fps and
0.15 at 60 fps over approximately 3.07 seconds each, with tracing enabled. These
are capture-path observations, not production video-workload benchmarks.
Actual OBS UI checks, physical DRM scanout/cursor integration, output resize,
and multi-GPU interoperability still require separate validation.

The full nested run with the Khronos layer reports
`VUID-vkAcquireNextImageKHR-fence-10066` during window presentation. The same
warning was reproduced with the previously installed `0fa1be8e` build in a
headless parent, before any capture requests. The swapchain-free render-node
regression above isolates capture synchronization from that presentation issue;
the full nested Vulkan validation run is not recorded as clean.


## Kooha region preview failures

On 2026-11-08, Kooha 2.3.1 region capture exposed a portal-wlr buffer ownership
race, reproduced with both 0.8.4 and unmodified 0.8.5. During the initial
STREAMING → PAUSED → STREAMING transitions, the backend returned an unfinished
buffer with timestamp zero, then acquired a different buffer before the old
Wayland capture completed. That completion consequently queued the wrong
buffer. GStreamer then received timestamps separated by approximately 14 hours
(the host uptime), and its source thread waited in `gst_clock_id_wait` while
preview remained at Loading. Increasing the buffer pool does not fix ownership.

The local compatibility patch
[`keep-ext-capture-buffer-on-pause.patch`](../scripts/portal-patches/keep-ext-capture-buffer-on-pause.patch)
retains the ext-image-copy buffer across pause/resume until its existing capture
completes. It preserves the older wlr-screencopy path. It applies to 0.8.4 and
0.8.5. This is an external backend patch, not a change to Weld or a claim that
upstream has accepted the fix. Build a patched backend and pass its executable
via `WELD_TEST_PORTAL_BACKEND` to the headless test. For a Nix package override:

```nix
pkgs.xdg-desktop-portal-wlr.overrideAttrs (old: {
  patches = (old.patches or []) ++ [
    /path/to/weld/scripts/portal-patches/keep-ext-capture-buffer-on-pause.patch
  ];
})
```

Patched 0.8.4 and 0.8.5 completed the headless selection-to-recording workflow,
producing cropped, decodable videos with changing pixels. These package builds
and tests do not replace or restart the installed desktop portal service.

A separate live observation captured the reported sustained 100% CPU after
selection: Kooha PID 117773's `pipewiresrc0:sr` thread consumed 2.00 CPU-seconds
in a two-second sample. Its stack was `gst_video_rate_transform_ip` →
`gst_video_rate_swap_prev` → `gst_video_rate_flush_prev` → buffer copying.
The UI thread waited in `gst_base_transform_activate` while shutting down the
preview pipeline. GStreamer 1.28.7's duplication loop in `swap_prev` ignores
flush errors. This establishes where the live CPU and UI stall occur, but the
live frame timestamps were not captured. The link between that loop and the
portal race remains to be verified on the physical session; passing nested
tests alone does not establish that the live 100% CPU symptom is fixed.

Evidence is retained in `target/validation/kooha-live-{cpu.json,stacks.log}`;
`output-capture-q7x4yyk1` and `output-capture-ez042w2s` show the unpatched
preview stall; `output-capture-n4na86io` and `output-capture-ok92bk5f` contain
passing cropped recordings from patched 0.8.5 and 0.8.4 respectively.
The final installed-library regression `output-capture-31zu6qyl` produced a
606×396 video with 124 decoded frames. `output-capture-i5d_5hmv` additionally
passed the 90-frame DMA-BUF portal test with no corrupted buffers and the
whole-output Kooha motion check using patched 0.8.4.


Window capture was verified in the private nested host on 2026-11-08
(`output-capture-maw5v2gs`), using the compatibility-patched portal 0.8.4:

- The 940×620 terminal viewport and 238×230 GPU-backed GTK viewport matched
  exactly between SHM and DMA-BUF capture; the output itself was 960×640.
- A live window-capture session renegotiated to 400×300 after resize, then
  emitted `stopped` and removed its foreign handle after the window closed.
- The portal delivered 90 complete DMA-BUF frames for a window source.
- Kooha advertised `MONITOR | WINDOW`, selected a `WINDOW` source on an inactive
  workspace, and recorded a 696×494 WebM with 119 decoded frames and verified
  changing pixels. The target's desktop visibility remained false at selection.
- Shared phone-view geometry / input routing tests and independent callback
  ownership tests passed, including release of capture's claim while another
  consumer still owns a claim.

The additional `--xwayland` run (`output-capture-dvoacdcw`) verified a 464×620
X11 application's SHM/DMA-BUF equality and all four expected corner colors.
It uses the private XWayland server through `weldctl run --shared-display`.
The isolated GPU destination-reuse regression also passed after generalizing
capture submission. The build-graph check still reports identical shared
features across desktop application, tests, examples, and Clippy.

These are headless nested tests; physical-session OBS UI checks still need
separate validation. The live Kooha/GStreamer loop described
above remains an external validation item for the next installed session.


The final run (`output-capture-cwo00sg1`) repeated native, GTK, XWayland,
resize/close, portal, and hidden-window motion checks, and delivered 180 window
frames at 60 fps with **zero additional desktop compositions**. Capture preserves
native callback ownership for visible windows; all six callback-policy tests pass.
