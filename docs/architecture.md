# Architecture

This document records the repository's implemented ownership and lifecycle
boundaries. The subject-oriented [Weld specifications](spec/README.md) preserve
project intent and future direction without presenting it as current behavior.

Weld is a workspace of reusable layers and one standard distribution:

- `weld-core` owns Smithay, Wayland protocol state, native input sources,
  backend event loops, DMA-BUF ownership, and native presentation adapters. It
  has no Bevy dependency.
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
- `weld-hoist` owns the current same-process hoist session, source placeholder,
  reclaim lifecycle, and loopback receiver. It does not own a network
  transport, media codec, or the receiver's ordinary window presentation.
- `weldwm` is the standard distribution. It requests a backend, configures the
  `WeldApp` returned by the builder with plugins and shortcuts, and supplies
  the executable. It is one possible assembly of the reusable crates, not the
  owner of their implementation.

Dependencies point inward: `weld-app` depends on `weld-core`; `weld-window`
depends on `weld-app`; the UI and floating-policy crates depend on the window
domain rather than on each other; and the distribution composes the complete
set. Core must not depend on Bevy, and the application or policy crates must
not depend directly on Smithay. A custom distribution can retain
`weld-window` while replacing `weld-window-ui`, `weld-ssd`, `weld-float`,
`weld-hoist`, or any combination of them, or build a different application
host while retaining the native backend and protocol machinery.

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

`weld-app` represents application-visible outputs as `WeldOutput` entities.
`OutputGeometry` carries pixel size and logical scale. `OutputPosition` locates
the output in compositor-wide logical space, while the separate
`OutputPlacement` carries its scale-independent physical footprint in
millimeters and records whether those dimensions were measured or assumed.
One entity is currently marked `PrimaryOutput`.
The shell's composition camera relates to it through `RendersOutput` and
`OutputCompositionCamera`, giving plugins a Bevy entity to use with
`UiTargetCamera` without exposing a native texture or wgpu handle.

Plugins may continue to spawn ordinary UI roots without selecting a camera.
Weld marks the primary output's composition camera as Bevy's
`IsDefaultUiCamera`, so normal UI targets the current compositor output
automatically. Output-specific roots select another output's camera with
`UiTargetCamera`; exactly one composition camera remains the default for
otherwise untargeted plugin UI. Weld's narrow Bevy patch also propagates the
scale of each manual texture-view target into that camera, leaving global
`UiScale` at `1.0`.

Managed windows carry a `WindowOutput` relationship, and `WindowGeometry` is
expressed in that output's local logical coordinates. `weld-float` assigns new
unassigned windows to the primary output, preserves explicit assignments, and
places each newly managed window within its assigned `OutputGeometry`.
Thereafter geometry remains manager-owned and is not clamped after movement,
output resize, scale change, or orphan adoption. A window may therefore remain
partly or entirely outside an output until explicit recovery policy exists.
Changing `OutputPosition` does not disturb local window layout.
`WindowOutputIntersections` is derived from globalized window and output
rectangles after management runs. The primary CSD or SSD root remains the
authoritative presentation and keeps its entity and original camera target
when the window is re-homed. An output-targeted secondary projection of that
scene and its popups is maintained for every other intersection. This keeps
active pointer targets stable and prevents a re-home from temporarily placing
two copies on one camera. A floating window is re-homed when its center enters
another output; its local geometry is converted so the global position does
not jump. Intersection derivation currently scans managed windows each
application update; change-filtered topology invalidation is a later
optimization.

Exact intersection membership is forwarded to Smithay as `wl_output.enter`
and `wl_output.leave`. Client preferred scale follows the highest-scale
intersection, with an eight-logical-pixel penetration threshold before moving
from an existing lower-scale output to a newly intersected higher-scale output.
Popups currently inherit their root toplevel's membership and preferred output;
independent popup geometry policy is deferred.
All projections sample the same current client buffer, so mixed-DPI output does
not retain temporally different surface versions.

If an assigned output entity disappears, `weld-float` preserves the durable
window and its stacking, reassigns it to the primary output, and leaves its
local geometry unchanged even during an active move or resize interaction.
When no unique primary output exists, floating admission waits rather than
inventing an origin or silently claiming the window.

Core owns a process-stable `OutputId`, immutable `OutputHead` connector facts,
dependency-free logical geometry, a revisioned validated `OutputLayout`, and
collision-aware `OutputTopology`. An output head carries its connector name and
optional EDID physical dimensions separately from mutable logical layout.
Pointer collision and portals follow shared edges in the logical layout.
Measured footprints remain available for diagnostics and calibrated layout;
missing dimensions use an explicit mode-derived 96-DPI footprint. EDID can
still be inaccurate.
The output domain is independent of a physical presenter. Logical layout,
physical footprints, collision portals, scale selection, output intersection,
and camera targeting remain Weld policy that a native adapter consumes. The
nested host supplies one host-window output. The standalone DRM host enables
all usable desktop connectors on the selected GPU at startup, chooses the first
internal panel by stable name as primary, and centers that primary below the
other outputs. Dynamic connector hotplug remains deferred.

The DRM adapter uses Smithay's `DrmOutputManager` and `DrmCompositor` for CRTC,
mode, swapchain, plane, atomic commit, page-flip, pause, and activation
lifecycle. One physical-desktop owner retains the manager and every output
compositor. Weld implements only the renderer seam that binds each
Smithay-leased explicit-modifier DMA-BUF to its matching Bevy output target.
Outputs whose deadlines align share one Bevy RenderApp pass; other outputs
remain independently paced. It does not use
Smithay's desktop window model or restore the removed low-level presenter. See
[Direct DRM presentation](drm-presentation.md) and the
[DRM output adapter plan](drm-rendering-improvement-plan.md).

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

Weld owns the outer host, Smithay server, event-loop orchestration, and native
presentation boundary. Bevy supplies its app schedule, renderer, UI primitives,
and BSN scene composition, rendering both client surfaces and shell UI through
Bevy's manual render-device path. The nested adapter presents a Weld-owned
texture through winit. A physical adapter will bind Smithay-owned output
allocations at the same application boundary. Do not enable Bevy's window
runner or expand its features without a concrete need.

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
extent, composition format, and DMA-BUF resources before Bevy is constructed.
That ordering prevents Bevy from selecting a second device. Preparation yields
a render context and a same-thread, one-shot runtime; the context is consumed
while constructing `AppShell` and is not retained across output resizes. The
resulting `CompositionHost` is a Bevy-free core contract: backends deliver
protocol-neutral surface and seat changes, advance application policy, request
composition into an owned or backend-leased target, and collect protocol
actions. `AppShell` is the standard Bevy implementation and owns the retained
offscreen target, but the core does not require it.

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
frame. Every committed use has its own release identity even when several uses
refer to that shared imported image. The server still groups the legacy
`wl_buffer.release` event by buffer and emits it only after every submitted use
has completed.

When the Vulkan render node supports syncobj eventfd notification, Weld also
advertises `linux-drm-syncobj-v1`. Smithay validates each explicit-sync commit.
The acquire and release points remain core-owned protocol state and never cross
into the application or plugin APIs. Hardware or clients without that protocol
continue through implicit synchronization.

Client implicit fences and explicit acquire points become Smithay commit
blockers registered with calloop. An explicit point uses syncobj eventfd;
otherwise the DMA-BUF's implicit readiness source is retained. Clearing either
blocker reapplies the held commit in the normal host iteration, which drains
the resulting surface changes and composition demand without polling or
waiting for unrelated Wayland traffic.

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

An explicit release point follows that individual committed use. A superseded
buffer that was never sampled signals immediately. A sampled use signals only
after the release submission containing its final GPU read completes. If two
commits reuse one imported image, the earlier use may therefore signal while a
later use remains displayed; the later use retains its own unsignaled point.
Destroying the `wl_buffer` resource suppresses only the eventual legacy release
event and does not discard outstanding explicit release points. Rejected
buffers, unsupported DMA-BUF cursor surfaces, and other never-sampled commits
signal immediately so a client cannot be stranded on a point Weld does not
own.

Prepared surface-material bind groups are cached by material, stable imported
image, sampling parameters, and resource generation. The cache retains the
entries for every still-live member of a rotating client buffer pool and
evicts them when the material changes, the selector disappears, or the client
destroys the buffer. Promotion is transactional: the surface registry publishes
a pending image only after native acquisition and GPU-image installation both
succeed. Failure retains a compatible previously displayed image, or the
transparent selector when no compatible image exists.

A persistent completion worker waits for release-barrier `SubmissionIndex`
values and wakes calloop. The server thread then signals any explicit timeline
point with a CPU syncobj ioctl and sends `wl_buffer.release` when the buffer's
last use has completed. The GPU wait remains off the compositor thread; there
are no timers, status polling, per-frame threads, or Wayland resources in the
worker. Importing a native Vulkan completion fence directly into the release
point is a possible refinement, not part of the current correctness contract.
The first acquire uses `GENERAL` as the producer-owned layout, following the
Wayland/Vulkan compositor convention for an initialized external image.
Running this path with Vulkan validation layers is a release gate once those
layers are available in the development environment.

Every `PendingDmabufFrame` must either be staged into `DmabufManager` or passed
to `release_unrendered`; it intentionally carries no protocol or channel type
above core. It does not yet have a `Drop` fallback, so a future hardening pass
may replace its opaque identity with an internal RAII completion token without
changing the plugin-facing boundary. Violating this invariant also retains an
explicit release point and its timeline import device, not only the legacy
buffer release, so that hardening has value beyond client responsiveness.

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
policy; it does not own window placement, stacking, or decoration. The nested
final wgpu pass presents or captures Bevy's completed texture. The application
keeps a stable manual texture-view handle so a future physical adapter can
substitute a Smithay-leased output allocation without retargeting cameras, UI,
picking, or plugins. An owned target remains necessary for capture, headless
operation, streaming, and composition while a physical session is inactive.
The removed DRM presenter is not a fallback. The DRM host substitutes a
Smithay-leased view while active and the same output's owned view while
inactive or capturing. Detailed follow-up sequencing is tracked in the
[DRM output adapter plan](drm-rendering-improvement-plan.md).

Demand-driven composition and client frame callbacks remain independent of
physical output availability. Startup, first client mapping, and structural
shell changes use a bounded settling sequence because Bevy main-world, layout,
extraction, asset preparation, and render work need not converge in one pass.
Ordinary client commits request one composition and do not turn Weld into a
continuous renderer.

`weld-core` owns backend-neutral cursor configuration, Smithay cursor-surface
lifecycle, and normalized client cursor pixels. `weld-app` exposes reloadable
`CursorSettings`, interprets Bevy's standard `CursorIcon`, and accepts transient
`CursorRequest` overrides. Nested mode delegates final cursor presentation to
the host window system. DRM mode normalizes named and client cursor images into
per-output Smithay `MemoryRenderBuffer` elements. The global pointer is
projected into each output's local coordinates, so a cursor visual intersecting
a seam can be considered on both outputs. Smithay chooses each GBM cursor
plane; the existing composition blitter supplies the GPU fallback. Cursor-only
motion does not dirty the Bevy scene.

Raw input is forwarded to the focused client in order and retained for the next
refresh-paced application update. That contract lets client delivery run at
device-event pace without running Bevy schedules at the device polling rate.
The standalone libinput adapter preserves accelerated and unaccelerated motion,
scroll phases, gestures, clickfinger policy, and timestamps as protocol-neutral
events. The DRM host connects it to Smithay's session and seat lifecycle while
the output compositor remains authoritative for KMS presentation.
Bevy remains authoritative for root/layer selection and shell interaction.
Smithay re-evaluates the selected surface tree's current input regions and
subsurface ordering for every raw pointer event. Crossing an application-owned
clip boundary or moving between Bevy layers becomes authoritative on the next
composition frame; motion within the published client layer uses the retained
affine mapping at raw input pace. A held pointer button suppresses
frame-published pointer-focus replacement; Smithay grabs themselves remain
authoritative over event delivery. Bevy pointer projection stays in the press
output's coordinate space while any button remains held so generic drag deltas
stay continuous across render targets. The final release is delivered in that
captured space, then the unchanged pointer position is immediately republished
on its current output so stationary picking does not retain the old target.

The libinput adapter preserves each device's default acceleration profile and
speed, configures supported tap and clickfinger behavior, and retains swipe,
pinch, hold, scroll, cancellation, and timestamps in backend-neutral events.
The eventual input-settings API must preserve global, device-type, and
device-specific locality. Nested mode continues to use motion transformed by
the parent compositor, and Winit does not expose the parent Wayland gesture
stream.
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

`WindowPresentationOverride` lets an optional presenter reserve one managed
window without teaching the default CSD or SSD plugins about that feature.
Those presenters revoke and suppress all of their primary, secondary, and
popup projections while an override is present. The owner may supply an
ordinary `PresentsWindow` tree or deliberately keep the window locally
unpresented while preserving its applied inset and outer-geometry contract.

Authoritative occupancy and active client policy are separate.
`WindowClientBinding` is absent for an ordinary window, suppresses client
policy on a replacement frame, or proxies another managed window's direct
occupant for one hop. `WindowClientResolver` applies that binding consistently
to presentation, popup ownership, focus, configure/resize, close, output
membership, preferred scale, and protocol move/resize. A direct occupant wins
unless suppressed; duplicate, dangling, cyclic, or chained proxies resolve to
no endpoint. Proxying never transfers `OccupiesWindow`, stable identity,
detach authority, or reclaim authority.

`weld-hoist` uses those contracts for a local loopback proof. `Super+H`
replaces the focused occupied window with a hoist-owned Reclaim scene and
suppresses its client policy while retaining its real occupant. It creates a
floating receiver bound to that source. The receiver uses the ordinary CSD or
SSD presenter, samples the same imported client image, owns popup attachment,
focus, configure/resize, CSD interactions, output membership, and preferred
scale, and follows later decoration changes. Reclaim or receiver loss removes
the receiver and binding while retaining the source window, geometry, and
occupant. Explicit reclaim is staged: the receiver is hidden, adopts the
source frame's output and requested inner size, and remains the policy endpoint
until that client configure settles or a bounded recovery deadline expires.
Only then does ordinary presentation return to the source. Adopting the remote
geometry directly would be simpler, but would discard the source frame's
layout reservation. The hidden receiver is intentionally deactivated and its
popups are hidden during this short transition; source focus and popup
presentation resume after handoff. Unexpected receiver destruction cannot use
the staging endpoint and therefore restores the source immediately.

`WindowClientBinding` is an in-process projection of that endpoint choice, not
a wire contract. A remote adapter must use stable Weld identities and separate
source-authoritative lifecycle from destination-supplied presentation
preferences, surface-family relationships, input, and media. This direct
same-process image sharing is not a media or wire contract: related
transient-toplevel relocation, cross-process transport, encoding, and remote
authorization are not implemented.

Core translates Smithay's `xdg_toplevel.set_parent` state into stable
`SurfaceId` parent metadata; no Wayland object crosses into the application
model. `WindowFamilyResolver` follows direct authoritative occupants through
that metadata and excludes proxy receivers. The local hoist plugin groups one
source/receiver session per independent toplevel under a shared family ID,
admits existing and later parent descendants while active, and reclaims the
captured family atomically. Members present when hoisting begins retain their
source layout slots and receive individual Reclaim placeholders. Members first
admitted while the family is already active are receiver-only because they
never occupied pre-hoist source layout. Destroying a captured member remotely
keeps its slot as a dismissible closed tombstone without a Reclaim action;
ordinary protocol unmap instead ends that member session so a later remap can
return through default presentation. Popups and subsurfaces remain within
their owning surface tree. A re-parented member is staged back to its source
rather than remaining disclosed through a family it has left. SSD treats any
generic proxy binding as a relocated presentation and uses focused and
unfocused red border shades without querying hoist-owned state.

Enabling Smithay's `desktop` feature for focused protocol utilities does not
make its `Window` or `Space` types authoritative for ordinary application
windows; their placement, stacking, presentation, and picking remain ECS-owned.
When `wlr-layer-shell` becomes a concrete implementation slice, prefer
Smithay's `LayerMap` as the host-side layout engine for anchors, margins,
exclusive zones, and configure state, then project its committed results into
ECS instead of reimplementing that protocol policy.

Validated pointer `xdg_toplevel.move` and `xdg_toplevel.resize` requests cross
the Smithay boundary as protocol-neutral ECS messages. The active window
manager consumes those requests directly, resolves their occupant, verifies
manager ownership, and decides whether to create or end an interaction.
`weld-float` records protocol-controlled lifetime separately from
pointer-controlled lifetime; Smithay's release-derived protocol end is the
only input fact that terminates a protocol-controlled session.

Presentation crates do not choose window actions. `weld-window` defines
passive `WindowMoveHandle`, `WindowResizeHandle`, and `WindowCloseHandle`
components. SSD and other presentations place those headless affordances on
their Bevy entities, while the active manager installs the pointer observers
that interpret them. `weld-float` currently binds primary press to activation,
move handles, and resize handles, and primary click to close handles. A
different manager can consume the same affordances with different policy.
This follows Bevy's headless-widget and styled-presentation split without
depending on Feathers. `weld-window-ui` retains presentation feedback such as
resize cursor icons, but it emits no move, resize, focus, or close policy.

After accepting an interaction, a manager uses the neutral
`WindowCommand::BeginInteraction` and `WindowCommand::EndInteraction` commands
to publish the single queryable `WindowInteractionSession`. Begin commands are
manager-private by convention; the window primitive validates occupant state
and maintains exclusivity but does not read buttons or motion. `weld-float`
owns the selected input controller, translates frame-paced mouse motion into
the public manager intents `MoveBy` and `ResizeBy`, and ends pointer-controlled
sessions on the button it selected. Those intents are not mouse-specific:
keyboard, touch, gamepad, remote, and scripted systems can provide deltas to
the owning manager through the same boundary. Output re-homing, projection
replacement, and temporary occupant unmapping therefore cannot interrupt an
active interaction.

`weld-float` registers `Super+LMB` as move and `Super+RMB` as resize. Raw
ingress is the sole chord evaluator: it consumes a matching press and paired
release before client delivery, then retains the frontmost picked Bevy entity
and compositor-logical press position in `PointerShortcutPressed` for the next
application frame. Float policy resolves that entity through its
`WindowProjection`, activates an owned window, and starts the selected action.
Modifier resize follows Sway's quadrant rule, choosing one horizontal and one
vertical edge relative to the window's global center. The initiating button is
float policy, not part of the window primitive, so another manager may bind
left, right, middle, or another supported button differently.

Captured motion remains in the Bevy input batch but is withheld before Smithay
sees it; this is a narrow shortcut filter, not yet a native shell pointer grab,
and the next forwarded positioned event resynchronizes Smithay's seat
position. A client already holding another button may defer that
resynchronization until its grab ends. This applies to pickable descendants
such as client input nodes and SSD chrome; ignored roots, client-excluded input
regions, and CSD shadow overflow do not become shortcut targets. A consumed
shortcut over background, an overlay, or a window owned by another manager is
a shell-owned dead click and is not replayed to a client. The picked entity is
process-local application state and never crosses into Smithay or `weld-core`.
Equivalent touch interaction remains future work. Smithay owns protocol grab
validation, configure state, and enforcement of the client's committed size
constraints. Repeated
interactive-resize sizes are latest-value coalesced at the Smithay server
boundary and configured at most once per composition tick; pointer motion,
buttons, axes, gestures, and keyboard input reach clients without that pacing.
Physical output traversal changes only the compositor's absolute pointer
position. A future relative-pointer implementation must preserve the original
libinput accelerated and unaccelerated deltas and must not derive them from the
physical topology projection.
Click activation is observed on the next application frame, after Smithay may
already have established the ordinary implicit click grab. Core records the
positive owner of that ordinary grab so the matching activation may apply
immediately. An ordinary click on shell chrome has no client owner and permits
ECS policy to activate any concrete toplevel during that grab; clear-focus
still waits. Popup and protocol move/resize grabs clear the exception and keep
their normal grab authority. Ending a resize can fold its latched final size
and the cleared `Resizing` state into one final configure. Destruction and close
requests discard any latched size; future maximize or fullscreen policy must do
the same before issuing its own configure. The window domain records the
surface commit revision at each client resize request. For left and top edges,
`weld-float` retains the fixed edge in a private settlement anchor as the live
interaction session ends. That anchor remains until the revision
advances, regardless of whether a constrained client commits the exact
requested size, and is discarded if its occupant unmaps. Client-issued
protocol move and resize requests
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
