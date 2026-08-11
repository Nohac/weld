# Architecture

Weld is a workspace with three reusable layers and one standard distribution:

- `weld-core` owns Smithay, Wayland protocol state, native input sources,
  backend event loops, DMA-BUF ownership, and final wgpu presentation. It has
  no Bevy dependency.
- `weld-app` owns the Bevy application and render bridge, the plugin-facing
  application model, input projection, surface entities, and composition into
  a core-owned texture. Plugin APIs use Weld and Bevy types rather than
  Smithay protocol objects.
- `weld-window` is an optional policy crate that claims ordinary application
  surfaces and supplies Weld's default client- and server-decorated window and
  popup presentations.
- `weldwm` is the standard distribution. It selects a backend, constructs the
  Bevy `App` after the backend has opened its GPU context, chooses plugins and
  shortcuts, and supplies the executable. It is one possible assembly of the
  reusable crates, not the owner of their implementation.

Dependencies point inward: `weld-app` depends on `weld-core`, `weld-window`
depends on `weld-app`, and the distribution depends on all three. Core must not
depend on Bevy, and the application or policy crates must not depend directly
on Smithay. A custom distribution can replace `weld-window`, add application
plugins, or build a different application host while retaining the native
backend and protocol machinery.

`weld-app` keeps native host-ingress records behind its public surface facade.
The `test-support` feature exposes those records only so downstream policy
crates can exercise complete lifecycle behavior; distributions and plugins
must not enable it in production.

Weld owns the outer winit window or DRM session, Smithay server, event-loop
orchestration, and final wgpu presentation. Bevy supplies its app schedule,
renderer, UI primitives, and BSN scene composition, rendering both client
surfaces and shell UI into a Weld-owned texture through Bevy's manual
render-device path. Do not enable Bevy's window runner or expand its features
without a concrete need.

Bevy's public APIs remain pinned to 0.19, while the active rendering crates are
temporarily patched under `vendor/bevy-wgpu30` to use wgpu 30 as one coherent
type generation. This pin exists because wgpu 30 lets Weld tell the resource
tracker the initial state of an already initialized HAL texture. Weld uses that
API for its DMA-BUF import path. Remove the vendor tree, its provenance record,
and the root `[patch.crates-io]` section when Weld adopts a suitable Bevy
release that natively depends on wgpu 30 or newer; the import architecture does
not otherwise depend on the patch being local.

Weld accepts multiple xdg-toplevels backed by `wl_shm` or linux-dmabuf and
exposes their lifecycle through protocol-neutral ECS entities. SHM pixels are
copied into Bevy images. A DMA-BUF is imported as an external Vulkan image and
sampled directly by the private material behind `SurfaceNode`; the path has no
CPU pixel copy, GPU normalization blit, or intermediate surface texture.

The boundary has three distinct representations. Smithay emits a core-owned
surface snapshot whose changed layer is retained content, owned SHM pixels, or
a validated DMA-BUF plus an opaque release identity. `AppShell` translates
that snapshot and asks the core-owned DMA-BUF manager to resolve an external
image into a Bevy handle. Application plugins receive only retained content,
pixels, or a Bevy `Handle<Image>` with project-owned sampling metadata; they
never handle Smithay protocol objects, file descriptors, Vulkan images, or
wgpu resources. Adjacent application snapshots coalesce while carrying the
newest unobserved content.

The native backend opens the wgpu instance, adapter, device, queue, output
extent, composition target, and DMA-BUF resources before invoking the
distribution's application-host factory. That ordering prevents Bevy from
selecting a second device. The resulting `CompositionHost` is a Bevy-free core
contract: backends deliver protocol-neutral surface and seat changes, advance
application policy, request composition into a core-owned target, and collect
protocol actions. `AppShell` is the standard Bevy implementation, but the core
does not require it.

Linux-dmabuf is advertised at protocol version 6 only when the selected Vulkan
adapter exposes a DRM render node, external DMA-BUF memory, foreign queue-family
ownership, and at least one sampleable/importable format-modifier pair. The
first slice accepts one-plane ARGB8888, XRGB8888, ABGR8888, and XBGR8888 with
explicit modifiers and optional `Y_INVERT`; implicit modifiers, multiplane and
YUV formats, HDR formats, interlacing, and cross-GPU transfer are rejected. If
capability discovery fails, Weld omits the global and retains SHM rather than
advertising a path that can silently fall back.

Protocol creation performs the real Vulkan import before acknowledging a
DMA-BUF. Each live `wl_buffer` pins that imported Vulkan image and memory in a
shared source cache until the client destroys the buffer; commits reuse the
same import rather than duplicating its file descriptor and native objects per
frame. Multiple in-flight uses of one buffer share a release identity, and the
server emits one release only after every submitted use has completed.

Client implicit fences become Smithay commit blockers registered with calloop.
After readiness, each layer moves through staged, displayed, and retiring
states. Immediately before Bevy renders, Weld promotes the newest referenced
staged buffer, inserts its imported texture as a fresh Bevy GPU-image identity,
and submits a raw Vulkan foreign-queue acquire. The buffer stays acquired and
unreleased across every redraw that reuses it. When a replacement or removal
reaches ECS, the old image remains valid through that RenderApp submission;
only afterward does Weld submit its foreign-queue release. Queue submission
order therefore surrounds every possible Bevy read without modifying Bevy's
renderer. Superseded staged buffers were never sampled and need no ownership
transfer. Replacement, unmap, layer removal, and destruction all request a
composition; shutdown explicitly drains acquired buffers if that composition
cannot run. Vulkan ownership is tracked per imported image rather than per
surface layer: reattaching one `wl_buffer` or displaying it in multiple layers
shares one acquire, and the image is released only when its final displayed use
retires. Each protocol use still completes independently.

A persistent completion worker waits for release-barrier `SubmissionIndex`
values and wakes calloop; only the server thread then sends
`wl_buffer.release`. There are no timers, status polling, per-frame threads, or
Wayland resources in the worker. The first acquire uses `GENERAL` as the
producer-owned layout, following the Wayland/Vulkan compositor convention for
an initialized external image. Running this path with Vulkan validation layers
is a release gate once those layers are available in the development
environment.

Wayland ARGB channels are premultiplied in their encoded representation while
Bevy UI blends straight alpha. The surface material loads source texels from
the imported non-sRGB view, unpremultiplies encoded RGB (or forces alpha for X
formats), converts sRGB to linear, and returns straight alpha directly into
Bevy composition. Pixel-aligned 1:1 presentation uses one texel load within a
small alignment tolerance. Scaling, rotation, or subpixel placement loads four
neighboring texels, normalizes each independently, and interpolates in linear
space. Taps clamp to the complete client texture rather than a viewport crop,
matching Bevy's former image sampling. This makes a scaled translucent sample
more expensive than a normal Bevy image sample, but removes the full-surface
read, write, and later reread previously performed for every client commit.
The material's whole-buffer `Y_INVERT` and viewport-coordinate mapping execute
in WGSL and are covered by runtime visual validation. Duplicating that
coordinate expression in Rust would not verify the shader; a shader execution
or image-comparison harness is the appropriate automated coverage when Weld
adds one.

Readable subsurfaces above the toplevel root are ordered and positioned as
internal Bevy image layers behind the same project-owned `SurfaceNode`; the
root image stays on that node so its rounded clipping and root-only fast path
remain intact. The default window plugin independently claims and decorates
each mapped surface, so client content composes with ordinary Bevy UI. Smithay
remains responsible for Wayland protocol state and applies focus or close
actions chosen by ECS policy; it does not own window placement, stacking, or
decoration. The final project-owned wgpu pass presents or captures Bevy's
completed texture directly in both backends. Standalone DRM uses the Vulkan
display WSI validated by the retained probe and never constructs Smithay's
Pixman, GBM, or `DrmCompositor` presentation path. FIFO acquisition and
presentation run on an event-driven worker so libseat, Wayland, input, and ECS
remain responsive while a driver blocks. Bevy owns two identified composition
targets; the host never writes the target currently owned by that worker, and
pending compositions are bounded to the newest host-owned target. Physical
output availability does not gate demand-driven composition or client frame
callbacks. The DRM cursor is presentation metadata rather than a Bevy UI node:
cursor-only motion reuses the completed composition and updates the final wgpu
blit without running Bevy's render app. Pointer interactions that actually
change shell UI still request an ordinary composition. Weld advertises
`xdg-decoration` and answers decoration
objects with server-side mode. Creating a decoration object opts a client into
Weld's server-side frame; clients that do not bind the global retain their own
decorations and are presented without duplicate shell chrome. A late
decoration decision swaps the client- or server-decoration presentation while
the durable `AppWindow`, placement, stacking, focus, and backing assets remain
intact. The presentation root's entity identity is intentionally not stable.

Enabling Smithay's `desktop` feature for focused protocol utilities does not
make its `Window` or `Space` types authoritative for ordinary application
windows; their placement, stacking, presentation, and picking remain ECS-owned.
When `wlr-layer-shell` becomes a concrete implementation slice, prefer
Smithay's `LayerMap` as the host-side layout engine for anchors, margins,
exclusive zones, and configure state, then project its committed results into
ECS instead of reimplementing that protocol policy.

Validated pointer `xdg_toplevel.move` and `xdg_toplevel.resize` requests cross
the Smithay boundary as protocol-neutral ECS messages. The default window
plugin owns placement and interactive-resize policy, while Smithay owns the
pointer grab, configure state, and enforcement of the client's committed size
constraints. Left and top resize edges remain anchored to the size the client
actually commits. Pointer interactions are implemented; the equivalent touch
path remains future work. Client-issued protocol move and resize requests are
accepted only for client-decorated windows; Weld's chrome owns movement for
server-decorated windows, and SSD resize handles remain outside this slice.
Smithay does not currently expose a decoration-object destroy callback through
this handler API, so a toplevel remains server decorated after creating that
object until the toplevel itself is destroyed.

Committed `xdg_surface.set_window_geometry` defines the plugin-facing
`MappedSurface.logical_size` and the shell's placement and resize anchor. A
client-decorated presentation renders the full root surface, including visual
overflow outside that geometry. If such overflow exists, Weld treats it as a
client-owned shadow or similar flare and suppresses its fallback shadow; a CSD
surface without overflow receives the fallback. A server-decorated
presentation crops the client to its window geometry and uses Weld's frame and
shadow. Changing decoration ownership therefore changes the presentation's
visual origin without changing its durable geometry anchor.

XDG popups use Smithay's `PopupManager` for protocol trees, committed
positioner state, and explicit seat grabs, while each mapped popup has a
separate protocol-neutral `AppPopup` ECS role. Popup presentation reuses the
ordinary full client-surface tree, input regions, scaling, and client-owned
visual overflow beneath its owning window presentation. The parent presenter
publishes its client window-geometry anchor, so popup code does not depend on a
particular decoration implementation. Popups never receive `AppWindow`,
`WindowPlacement`, shell decorations, fallback shadows, or interactive
move/resize policy. The initial popup slice honors committed client positioner
geometry directly; output-edge flip, slide, and resize constraints remain a
bounded follow-up using the owner's on-output client geometry.

Explicit Wayland input regions are evaluated in protocol order and may extend
outside the window geometry, which keeps client-side resize gutters reachable.
For an undeclared root input region, Weld deliberately treats only the window
geometry as interactive even when CSD overflow is visible; this differs from
the protocol's full-surface default so transparent shadow margins remain inert.
Subsurfaces without an explicit input region use their full logical extent.
Picking targets identify the exact root or subsurface layer, and Smithay
revalidates that target before delivering input to the corresponding live
`wl_surface`. Geometry spanning subsurfaces outside the root buffer is not yet
represented. The private surface material is a project-owned presentation
detail, not the plugin-facing surface contract. Below-root subsurface ordering, role-only
subsurface detachment without a later tree commit, damage-aware uploads,
presentation timing, VRR, and HDR remain explicit spike boundaries rather
than settled compositor architecture.

Nested wheel input stays discrete, while Winit pixel scrolling is treated as a
finger gesture and retains its start, move, end, cancellation, and per-axis
stop lifecycle through Bevy projection and Wayland delivery. Axis frames use
Smithay's existing pointer focus rather than performing another hit test or
changing focus; focus changes remain the responsibility of pointer motion and
button events. Leaving the host window or losing host focus cancels any active
finger axes before clearing pointer state.

Ordinary rendering is event driven. Host and client-surface changes request a
composition directly; Bevy systems that drive continuous visual changes should
emit `bevy::window::RequestRedraw` while they remain active. An output refresh
rate is the upper presentation opportunity for a continuous stream, not a
requirement that every visible client submit a new buffer each refresh. Idle,
slow, or occluded clients reuse or retain their current buffer. Different
outputs, VRR, exclusive scanout, and headless or streaming consumers may expose
different cadences. Bevy primitives participate normally in a requested
composition, but their mutation is not a universal automatic invalidation
signal.

BSN and Bevy's UI work are references for composition, behavior, accessibility,
and state synchronization. Do not add Feathers by default. If Weld adopts Bevy
scene or headless-widget infrastructure, keep domain state authoritative
outside widgets and translate widget events into project-owned actions. We will
design a Weld-specific visual layer separately when a concrete UI slice exists.

Keep provisional decisions easy to reverse. Before making an architectural
change, describe the ownership, boundary, and semantics it establishes. Judge
pre-stable changes by whether they leave a coherent structure, not by diff
size. Prefer smaller incremental changes after the structure and compatibility
expectations have stabilized.
