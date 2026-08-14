# Architecture

This document records the repository's implemented ownership and lifecycle
boundaries. The subject-oriented [Weld specifications](spec/README.md) preserve
project intent and future direction without presenting it as current behavior.

Weld is a workspace of reusable layers and one standard distribution:

- `weld-core` owns Smithay, Wayland protocol state, native input sources,
  backend event loops, DMA-BUF ownership, and final wgpu presentation. It has
  no Bevy dependency.
- `weld-app` owns the Bevy application and render bridge, the plugin-facing
  application model, input projection, surface entities, and composition into
  a core-owned texture. Plugin APIs use Weld and Bevy types rather than
  Smithay protocol objects.
- `weld-window` owns UI-independent managed-window identity, occupancy,
  geometry, visibility, stacking, focus, interaction, and presentation
  contracts. Managed windows are distinct from the shorter-lived client
  surfaces that occupy them.
- `weld-window-ui` projects managed windows into unstyled Bevy UI roots. It
  supplies reusable client-surface mounts, client-decorated and popup
  presentation, presentation arbitration, and pointer-to-window-intent
  behavior.
- `weld-ssd` supplies Weld's current opinionated BSN server-decoration scene.
  It validates the presentation contract but is optional policy that another
  shell or window manager can replace.
- `weld-float` supplies conventional freeform placement, focus, stacking,
  movement, and interactive-resize policy without owning UI entities.
- `weldwm` is the standard distribution. It requests a backend, configures the
  `WeldApp` returned by the builder with plugins and shortcuts, and supplies
  the executable. It is one possible assembly of the reusable crates, not the
  owner of their implementation.

Dependencies point inward: `weld-app` depends on `weld-core`; `weld-window`
depends on `weld-app`; the UI and floating-policy crates depend on the window
domain rather than on each other; and the distribution composes the complete
set. Core must not depend on Bevy, and the application or policy crates must
not depend directly on Smithay. A custom distribution can retain
`weld-window` while replacing `weld-window-ui`, `weld-ssd`, `weld-float`, or
all three, or build a different application host while retaining the native
backend and protocol machinery.

The presentation split follows Bevy UI's separation of raw UI infrastructure,
unstyled reusable behavior, and opinionated Feathers scenes without depending
on Feathers itself. `weld-app` supplies the raw client-surface rendering
primitive; `weld-window` is the UI-independent application domain beneath the
analogy; `weld-window-ui` supplies reusable Node-based presentation behavior;
and `weld-ssd` supplies one styled BSN composition. Domain systems never query
SSD scene markers. Weld does not currently enable `bevy_ui_widgets`, because
its input-focus and dispatch plugins must first be reconciled explicitly with
compositor keyboard focus and global shortcut routing.

Distributions normally construct Weld through `WeldApp::builder()`. Building
resolves and opens the native backend and GPU first, then returns a wrapper
around the real Bevy `App`. The distribution may add ordinary Bevy plugins,
systems, and resources before calling `run()`. `ActiveBackend` is inserted
before plugin construction, and `WeldAppExt` lets any standard Bevy plugin
inspect it without requiring a separate Weld plugin trait. The low-level
`HostBuilder` and `CompositionHost` contract remain available for non-Bevy
application hosts; backend module entry points are implementation details.

Plugins spawn ordinary UI roots without selecting a camera. Weld marks its
single composition camera as Bevy's `IsDefaultUiCamera`, so normal UI targets
the compositor output automatically. Once Weld supports multiple outputs,
output-specific roots will select their camera with `UiTargetCamera`; exactly
one composition camera remains the default for otherwise untargeted plugin UI.

`weld-app` re-exports its exact supported Bevy version as `weld_app::bevy` so
plugins can share Weld's ECS, application, and rendering types without an
independent version choice. A plugin may depend directly on that same exact
Bevy release when it needs to enable an additional additive feature, but a
different Bevy version has incompatible types and builds a separate framework
artifact. Until Weld removes its temporary wgpu 30 compatibility patch, an
out-of-tree distribution must also carry the root patch configuration described
below; dependency patches do not propagate from a library crate.

The builder is bootstrap-only. Reloadable window, input, appearance, and other
policy settings belong in ECS resources, where Bevy change detection lets
systems observe replacements without recreating the application or losing
client and window state. A setting backed by live native state must eventually
cross the host boundary as a typed request; backend choice, GPU selection, and
other immutable roots require a deeper reinitialization or process restart.

The one-shot runtime does not yet implement application replacement or crash
recovery, but its ownership boundaries must leave that possible. Smithay and
the live Wayland socket and client connections belong to core rather than to
Bevy policy. Any future replacement flow should snapshot durable policy and
window state with Weld identifiers and project-owned data—not Bevy `Entity`
values or raw Smithay objects—then rehydrate a new application host against
the retained core connection state. Do not partially rebuild those roots as a
side effect of ordinary settings reload.

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

Surface entities describe protocol lifecycle, mapping, geometry, and input
structure. A buffer-only commit updates private surface and render resources;
it does not replace surface components. A resource-owned commit sequence is
kept separately for policies such as completing an anchored resize after the
next client commit. Each surface layer also owns a stable transparent selector
image. Materials keep that selector handle while the private render binding
chooses the currently displayed client image, so rotating a client buffer pool
does not appear as ECS or material identity churn.

`HostBuilder::prepare` opens the wgpu instance, adapter, device, queue, output
extent, composition target, and DMA-BUF resources before Bevy is constructed.
That ordering prevents Bevy from selecting a second device. Preparation yields
a render context and a same-thread, one-shot runtime; the context is consumed
while constructing `AppShell` and is not retained across output resizes. The
resulting `CompositionHost` is a Bevy-free core contract: backends deliver
protocol-neutral surface and seat changes, advance application policy, request
composition into a core-owned target, and collect protocol actions. `AppShell`
is the standard Bevy implementation, but the core does not require it.

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
staged buffer, installs its imported texture under the stable GPU-image
identity of that live `wl_buffer` if needed, and submits a raw Vulkan
foreign-queue acquire. All images acquired for one composition share one
command encoder and barrier batch. The buffer stays acquired and unreleased
across every redraw that reuses it. When a replacement or removal reaches the
application surface registry, the old image remains valid through that
RenderApp submission;
only afterward does Weld submit its foreign-queue release. Queue submission
order therefore surrounds every possible Bevy read without modifying Bevy's
renderer. Superseded staged buffers were never sampled and need no ownership
transfer. Replacement, unmap, layer removal, and destruction all request a
composition; shutdown explicitly drains acquired buffers if that composition
cannot run. Vulkan ownership is tracked per imported image rather than per
surface layer: reattaching one `wl_buffer` or displaying it in multiple layers
shares one acquire, and the image is released only when its final displayed use
retires. Each protocol use still completes independently.

Prepared surface-material bind groups are cached by material, stable imported
image, sampling parameters, and resource generation. The cache retains the
entries for every still-live member of a rotating client buffer pool and
evicts them when the material changes, the selector disappears, or the client
destroys the buffer. Promotion is transactional: the surface registry publishes
a pending image only after native acquisition and GPU-image installation both
succeed. Failure retains a compatible previously displayed image, or the
transparent selector when no compatible image exists.

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
remain intact. On first map, `weld-window` admits each client toplevel into a
distinct `ManagedWindow` and relates the short-lived surface entity as its
occupant. Presenters claim the managed window independently, so client content
composes with ordinary Bevy UI without making presentation-root identity or
surface lifetime authoritative for window policy. Smithay remains responsible
for Wayland protocol state and applies focus or close actions chosen by ECS
policy; it does not own window placement, stacking, or decoration. The final
project-owned wgpu pass presents or captures Bevy's
completed texture directly in both backends. Standalone DRM uses the Vulkan
display WSI validated by the retained probe and never constructs Smithay's
Pixman, GBM, or `DrmCompositor` presentation path. FIFO acquisition and
presentation run on an event-driven worker so libseat, Wayland, input, and ECS
remain responsive while a driver blocks. The current bootstrap allocates a
fixed pair of composition targets in `weld-core`. For each composition, core
hands `weld-app` the view that is free for rendering; the app binds that view to
its stable Bevy camera target. This is provisional ownership debt, not the
intended plugin-facing composition contract: core should expose GPU/output
capabilities and final presentation primitives, while the application layer
requests its render targets or layers, binds them to Bevy cameras, and chooses
how they compose. Core may enforce presenter ownership and back-pressure for
submitted targets, but it must not hardcode the application layer graph. Until
that boundary is refactored, the host never writes the target currently owned
by the DRM worker, and pending compositions are bounded to the newest
host-owned target.

Physical output availability does not gate demand-driven composition or client
frame callbacks. Startup, first client mapping, and structural shell changes
start a bounded settling sequence because Bevy's main schedule, layout, render
extraction, asset preparation, and GPU submission need not converge in one
pass. Ordinary client buffer commits request one composition and never extend
an in-flight settling sequence. Each completed intermediate composition is
eligible for immediate presentation. The fixed budget mirrors Bevy winit's
finite startup-update margin without turning Weld into a continuous renderer,
but remains a stopgap until Bevy exposes a reliable signal for pending deferred
or render-world work. Remote debugging services the main world at a bounded
maintenance rate and on other host wakes, but does not itself create
composition demand. The DRM cursor is presentation metadata rather than a Bevy
UI node: cursor-only motion reuses the completed composition and updates the
final wgpu blit without running Bevy's render app. Pointer interactions that
actually change shell UI still request an ordinary composition. `weld-core`
owns the Bevy-free cursor model, Smithay cursor-surface lifecycle, Xcursor
discovery, immutable GPU uploads, and final composition geometry. `weld-app`
exposes the reloadable `CursorSettings` ECS resource; replacing that resource
changes the theme or logical nominal size without exposing Smithay or wgpu to
plugins. Weld also interprets Bevy's standard `CursorIcon` component on the
hovered UI entity or its ancestors. Systems that need a transient global
override publish `CursorRequest` each update before `CursorSystems::Resolve`.
`weld-window-ui` uses those primitives to install directional shapes on resize
handles and retain the corresponding shape during an active resize. Client
cursor requests remain authoritative only while Smithay owns pointer focus;
Weld UI intent takes over as soon as the pointer returns to shell chrome.

Standalone mode advertises `wp_cursor_shape_manager_v1` and honors hidden,
named, and legacy client-surface cursor requests. Named shapes resolve through
the configured raster Xcursor theme, including theme inheritance and animation.
The DRM dispatch deadline includes the next animation frame only while the
session and output are available, so an idle, paused, or disconnected cursor
does not introduce polling. SHM cursor surfaces are copied at the Smithay
boundary, unpremultiplied in their encoded BGRA representation, and normalized
to the compositor's configured logical size. This intentionally prevents a
client-provided bitmap from changing the user's cursor size. Client DMA-BUF
cursor surfaces are not imported yet: Weld releases them, warns, and displays
the configured default shape rather than creating a second ad hoc DMA-BUF
ownership path. Scalable cursor-theme assets and hardware cursor planes are
also future work.

The final pass samples cursor pixels as sRGB, premultiplies each linear texel
before interpolation, and composites them over Bevy's premultiplied output.
Pixel-aligned 1:1 cursors use a single texture load; scaled or subpixel cursors
use four linear-space taps. Published cursor textures are immutable because
the DRM presenter may retain and requeue a frame. The presenter worker writes
the matching geometry uniform immediately before its submission. Nested mode
continues to use the host window-system cursor and binds a transparent cursor
to Weld's final blit. `CursorSettings` theme and size changes are therefore
inert in nested mode, although Bevy `CursorIcon` and `CursorRequest` shape and
visibility changes still reach the host cursor. Screenshots and remote captures
currently read the Bevy composition before the DRM cursor blit and therefore
exclude the cursor; that is deliberate so future streaming can carry cursor
metadata independently.

Standalone input additionally publishes the newest raw compositor-logical
pointer position to the cursor presenter before ECS picking and protocol
routing complete, then offers a cursor-only frame against the last completed
composition. The same ordered event still passes through the ordinary ECS
pipeline, and any resulting focus or shape change can replace the queued frame.
This removes avoidable application-schedule latency, but a software cursor can
still trail a hardware cursor plane by up to the display/presentation latency;
hardware-plane support remains the path to eliminating that final bound.

Standalone input preserves each libinput device's default acceleration profile
and speed. The eventual input-settings API must scope overrides per device or
device type. Nested mode continues to use motion already transformed by the
parent compositor.

Weld advertises `xdg-decoration` and answers decoration
objects with server-side mode. Creating a decoration object opts a client into
Weld's server-side frame; clients that do not bind the global retain their own
decorations and are presented without duplicate shell chrome. A late
decoration decision swaps the client- or server-decoration presentation while
the durable `ManagedWindow`, desired geometry, stacking, focus, occupant, and
backing assets remain intact. Presentation insets adjust outer desired geometry
by their delta, preserving desired client content size without configuring a
client solely because chrome changed. The presentation root's entity identity
is intentionally not stable.

Enabling Smithay's `desktop` feature for focused protocol utilities does not
make its `Window` or `Space` types authoritative for ordinary application
windows; their placement, stacking, presentation, and picking remain ECS-owned.
When `wlr-layer-shell` becomes a concrete implementation slice, prefer
Smithay's `LayerMap` as the host-side layout engine for anchors, margins,
exclusive zones, and configure state, then project its committed results into
ECS instead of reimplementing that protocol policy.

Validated pointer `xdg_toplevel.move` and `xdg_toplevel.resize` requests cross
the Smithay boundary as protocol-neutral ECS messages. `weld-window` validates
the occupant and owns the UI-neutral interaction session, `weld-window-ui`
translates pointer motion into window intents, and `weld-float` owns placement
and interactive-resize policy. Smithay owns the pointer grab, configure state,
and enforcement of the client's committed size constraints. Repeated
interactive-resize sizes are latest-value coalesced at the Smithay server
boundary and configured at most once per composition tick; pointer motion,
buttons, axes, and keyboard input still reach clients without that pacing.
Pointer-button effects are applied after ECS surface actions, so
ending the grab can fold its latched final size and the cleared `Resizing` state
into one final configure. Destruction and close requests discard any latched
size; future maximize or fullscreen policy must do the same before issuing its
own configure. Client-focus reconciliation also runs during input-only main
updates. A click activation therefore emits its focus action in the same batch
as the pointer press, and the host applies that action before Smithay establishes
the matching implicit click grab. The window domain records the surface commit revision at each
client resize request. Left and top resize edges remain anchored until that
revision advances, regardless of whether a constrained client commits the
exact requested size. Pointer interactions are implemented; the equivalent
touch path remains future work. Client-issued protocol move and resize requests
are accepted only for client-decorated windows; Weld's chrome owns movement
for server-decorated windows, and SSD resize handles remain outside this slice.
Client-decorated applications also own the threshold for deciding that a press
has become a titlebar drag. Before the client sends `xdg_toplevel.move`, Weld
cannot distinguish that intent from clicking any other client-owned control.
Weld therefore neither predicts the move nor replays the pre-request distance;
this matches the observed Sway behavior and avoids snapping when the grab
begins.
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
separate protocol-neutral `ClientPopup` ECS role. Popup presentation reuses the
ordinary full client-surface tree, input regions, scaling, and client-owned
visual overflow beneath its owning window presentation. The parent presenter
publishes its client window-geometry anchor, so popup code does not depend on a
particular decoration implementation. Popups never receive `ManagedWindow`,
`WindowGeometry`, shell decorations, fallback shadows, or interactive
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
