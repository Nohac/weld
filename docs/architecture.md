# Architecture

This document records the repository's implemented ownership and lifecycle
boundaries. The subject-oriented [Weld specifications](spec/README.md) preserve
project intent and future direction without presenting it as current behavior.

Weld is a workspace of reusable layers and one standard distribution:

- `weld-client` defines the runtime-independent client adapter, surface,
  buffer-lease, request, and input contracts. It has no Smithay, Bevy, wgpu,
  codec, or transport dependency.
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
- `weld-hoist-protocol` owns the serializable, transport-neutral hoist record
  subset shared by current bindings: exact revision and session identities,
  source and destination envelopes, surface modes, and
  encoded access-unit metadata. It contains no compositor, application,
  codec-backend, or transport implementation.
- `weld-hoist-core` owns transport-independent hoist identities, source and
  destination relay policy, and the Bevy-free loopback binding. Its relay
  ports hide framing, serialization, native handles, codecs, queues, and wake
  integration. The relays own surface admission, session authorization,
  source-authoritative identity relocation, popup and parent relationships,
  scale reset, focus, and remote-input cleanup.
- `weld-hoist-encoded` owns transport-neutral encoded commit scheduling,
  local backpressure, resize coalescing, codec worker contracts, and atomic
  decoded-frame publication. Its default dependency graph has no compositor or
  native graphics dependency. The optional `native` integration supplies Linux
  buffer preparation and DMA-BUF publication; `vaapi` additionally supplies the
  hardware codec binding. Port policy and publication run on the caller thread;
  native codecs run on bounded workers. Bindings move only control records and
  compressed access units.
- `weld-hoist-local` owns the Linux-local Postcard/Unix-seqpacket binding,
  SCM_RIGHTS native-buffer transfer, optional opaque encoded-media binding,
  and source/destination relay ports. It depends on core's native import
  capability but has no Bevy dependency. Its `encoded-vaapi` feature is
  optional for native-only library consumers.
- `weld-hoist-iroh` owns the authenticated Iroh endpoint host, peer
  connections, endpoint-ticket rendezvous, bounded Postcard framing, and the
  independent QUIC control and encoded-media streams. Iroh and Tokio types do
  not cross its registration boundary. Its default graph is compositor-free;
  optional native/VA-API registration uses the same portable peer machinery.
- `weld-hoist-ui` owns source placeholders, reclaim and closed-tombstone UI.
- `weld-hoist` owns Bevy window-family admission and reclaim orchestration. It
  also owns the process-local endpoint registry used to select an endpoint for
  each new family. It does not own client buffers, ordinary receiver
  presentation, a network transport, or a media codec.
- `weld-media` owns transport- and platform-neutral media identities and
  encoded payload contracts. Its additive `decode` feature owns bounded,
  stream-affine worker execution with backend-defined requests and outputs.
  It owns no native graphics API, codec backend, transport, Smithay, or Bevy
  object. Consumers of only the media records do not enable worker dependencies.
- `weld-media-vaapi` owns Linux FFmpeg/VA-API capability discovery and hardware
  media stages behind `weld-media` contracts. FFmpeg and libva types do not
  cross that boundary. Its native decoder processor runs inside
  `weld-media::decode::DecodePool`; native encoding remains in this crate.
- `weld-media-android` owns FFmpeg/NDK decoding to acquired native-image leases.
  Its single-producer target and retained ImageReader ownership are independent
  of Godot, transport and the decoder context lifetime. Both the diagnostic probe
  and Godot's Android provider use it. Portable codec configuration lives in
  `weld-media`'s light `config` feature.
- `weldwm` is the standard distribution. It requests a backend, configures the
  `WeldApp` returned by the builder with plugins and shortcuts, and supplies
  the executable. It is one possible assembly of the reusable crates, not the
  owner of their implementation.

Dependencies point inward: `weld-core` implements the local Smithay adapter
through `weld-client`; `weld-app` depends on both; `weld-window` depends on
`weld-app`; the UI and floating-policy crates depend on the window domain
rather than on each other; and the distribution composes the complete set.
Core, hoist protocol, and hoist core must not depend on Bevy, and application
or policy crates must not depend directly on Smithay. A custom distribution can retain
`weld-window` while replacing `weld-window-ui`, `weld-ssd`, `weld-float`,
`weld-hoist`, or any combination of them, or build a different application
host while retaining the native backend and protocol machinery.

The local, Iroh, and loopback bindings enter the same relay implementation.
Loopback contributes only in-process queues, buffer-lease relay, and route
aliases; the local binding contributes Postcard, Unix descriptors, DMA-BUF or
SHM import/export, and calloop wakes; and the Iroh binding contributes QUIC
connectivity, framing, and network wakes. Both encoded bindings use the same
`weld-hoist-encoded` ports rather than implementing another client-surface or
codec lifecycle.

`weld-media-vaapi` uses FFmpeg's H.264 and AV1 VA-API codecs for production.
The old cros-codecs H.264 implementation and its local patches remain available
only through the crate's `diagnostic` feature so the independent GStreamer
comparison stays reproducible; a default Weld build does not link it.
cros-libva remains a direct dependency for capability queries, DMA-BUF import,
and VPP because its surface ownership API is useful independently of its codec
stack.

FFmpeg owns a DRM hardware-frame context for source DMA-BUFs, derives one
VA-API device per encoder worker, supplies it to `hwmap`, and converts RGB into
limited-range NV12 with `scale_vaapi`. One persistent encoder and decoder pair
belongs to each media stream generation. Encoder and filter time bases are
microseconds, matching the transport timestamp. Both hardware encoders use
`async_depth=1`, no B frames, a one-second CBR reservoir, and a capacity-one worker;
the hardware round-trip example rejects a driver or FFmpeg build that does not
emit exactly one packet for each submitted frame. This lets Weld retain the
source client-buffer lease until its matching packet exists without an
unbounded delayed-frame table. The CBR reservoir controls rate accounting; it
does not add a software frame queue or frame reordering.

Encoded source admission is paced in `weld-hoist-encoded`, independently of
local composition and identically for local and Iroh transports. A presenter's
`SetPresentation` preference selects the encode cadence, clamped only when the
backend supplies an operating ceiling. The VA-API backend defaults to 60 fps
without a presenter preference; that bootstrap is not a ceiling. Explicit 90 or
120 Hz claims configure both source admission and FFmpeg at that rate. The
validated 1..1000 Hz protocol range is not a promise of hardware throughput;
device-specific cadence limits have not been probed. Full capability/answer
feedback to the destination remains separate. Requested and accepted rates,
and the settings of each successfully opened encoder generation, are logged
on the source under `weld_media_diag`.

Cadence is frozen with bitrate and extent for each admitted multi-layer batch.
A changed cadence replaces the encoder generation on the next admitted buffer
replacement, after existing prepared jobs finish; simultaneous bitrate/size
changes share that replacement. MilliHz preferences become reduced FFmpeg
rationals without rounding to integer fps, while timestamps retain their
microsecond time base. The GOP remains 32 **frames**, not a fixed duration, and
the bitrate allocator is unchanged: higher cadence at the same target bitrate
means fewer nominal bits per frame and more keyframes per second. The AV1
8 Mbps per-stream workaround remains in force.

Frame slots are per surface-tree snapshot, not per input event or layer. New
commits replace superseded not-yet-encoded content with the existing complete
inventory merge; encoded reference dependencies are never discarded arbitrarily.
The first pending frame can start immediately, subsequent admissions follow the
selected cadence, and missed whole intervals do not accumulate catch-up credits.
No work is generated for an idle surface. Caller-driven adapter deadlines reduce
the shared native runtime's dispatch timeout, so the final coalesced frame does
not wait for another client commit, input event, local redraw or 1s maintenance
tick. Worker completion and transport-capacity wakes remain responsible for their
own blockers. Finite cadence exclusions retain weighted-scheduler service debt;
fully excluded groups do not accrue starvation age.

Input and cursor traffic is not paced by video admission. Explicit callback
claims are clamped to the accepted rate, while a rate-less claim keeps the
source-output fallback (VA-API encode bootstrap defaults to 60 fps).
This does not force application commit cadence: other presentation owners and
clients committing without frame callbacks can still produce more work.
Paused streams retain bounded latest unobserved buffer leases for correct resume;
they have no periodic encode deadline. Full unmaps discard unpublished pixels,
preserve intervening control order, and suppress cancelled in-flight output;
in-flight source leases still await codec completion. Unmaps carrying retained
buffer inventory drain their ordered dependencies without a cadence wait,
retaining the last active encoder cadence across pause/release instead of
rebuilding at the bootstrap rate.

This corrects measured overproduction: a large headless Blender window supplied
about 210 encoded frames/s to a receive/decode path completing about 140-150/s,
reaching 126 pending **destination media frames** and roughly 1s pre-decode wait.
The normal source also overproduced, with a smaller measured backlog. Adaptive
lowering and recovery of the selected rate based on observed encode/decode
performance, aggregate codec throughput budgets, and destination rate-feedback
UI remain separate slices. An advertised/configured ceiling is not a guarantee
that every resolution or concurrent workload can sustain it.

`weld-hoist-encoded` owns an optional bitrate actuator for live layer streams.
Its weak `EncoderRateControl` is safe to retain in the thread-safe concrete
local/Iroh endpoint handles; only the host selects settings and submits codec
work. Requests coalesce, prepared jobs keep frozen settings, and bitrate changes
use the same serialized generation replacement as extent changes. Short control
locks never span codec work. Requested/submitted/applied revisions distinguish
intent from matching codec output, not remote presentation or measured bitrate.
Retirement removes control entries and owner closure invalidates handles.
VA-API supports lowering and restoring the validated startup rate by constructing
a replacement encoder, not by mutating a live context. The distribution now
injects one host-owned `SharedBitrateBudget` across its encoded source streams;
custom multi-port distributions can share the same handle. It distributes desired
encoder targets by activity-weighted presentation group and buffer area, outside codec and
transport ownership. It does not police wire throughput or adapt to congestion.
Trusted activity is shared with queue scheduling, but quality holds and atomic
focus settling suppress repeated encoder replacements. Input timestamps do not
change structural inventory; allocation follows the complete authorized input
batch, before admitting another encode. Expiry uses ordinary shared-budget work,
not a new timer or synthetic client commit.
See [shared encoder targets](shared-bitrate-targets.md) and the
[streaming-budget plan](remote-budgeting-plan.md).

Hardware-device ownership is per worker rather than per stream generation. The
encode worker opens one FFmpeg DRM device and derives one FFmpeg VA-API device;
each lazily created decode worker opens one FFmpeg VA-API device.
Generation-specific filters and codecs retain references to those devices
instead of reopening the render node during resize. The cros-libva VPP converter
owns one additional VA display in each worker. The hardware round-trip probe
rotates 16 encoder generations
and requires the process descriptor count to remain unchanged.

Encoder coded extents come from the selected VA profile and entrypoint's
reported minimum and maximum surface attributes. Missing AV1 minima use a
guarded 128x128 fallback established by the radeonsi failure; a missing H.264
minimum means no padding. Visible geometry above the maximum fails before the
codec opens. Smaller valid windows retain exact transported visible geometry
while `pad_vaapi` expands only their coded NV12 storage.
AV1 coded dimensions are then rounded upward to even numbers and checked
against the maximum again, avoiding an observed Mesa VCN padding-bound defect.
This does not change visible geometry or H.264 sizing. Driver evidence,
upstream status and guard removal criteria live in
[VA-API workarounds](vaapi-workarounds.md).

Run the bounded H.264 and AV1 production round trip with
`scripts/run-vaapi-roundtrip-probe`. It lives in `weld-media-vaapi` so codec
validation does not build the Bevy distribution dependency closure.

The decoder selects FFmpeg's native H.264 or AV1 parser so its `get_format`
callback can require VA-API output. A decoded AVFrame stays alive while Weld
exports its VASurface as a composed PRIME descriptor and synchronously
VPP-converts it into fresh XRGB storage at the authoritative transported
visible extent. Duplicating an exported descriptor alone is not considered
sufficient lifetime because FFmpeg may recycle the underlying decode surface.
Only the fresh XRGB allocation escapes the media worker.

The decode pool is reusable separately from that native implementation through
`weld-media`'s `decode` feature. `DecodeJob` exposes only stable job/frame identity;
`DecodeProcessor` owns its concrete request and output representations. Its
factory creates each processor on its worker thread, and the processor need
not implement `Send`. Only requests and outputs cross the queue boundary. The
pool itself remains movable between host threads. Existing limits, stream
affinity, FIFO pipelining, retirement acknowledgements and wake behavior are
shared rather than reimplemented by a platform frontend.

This is a low-delay execution boundary: completing a job must make bounded
progress without requiring a future submission, and there is no idle decoder
polling. Backend outputs must retain their storage independently of subsequent
decodes, retirement or backend destruction. Unpublished outputs may be dropped
after the backend, so releasing them must not require that backend to remain
alive. These are backend obligations, not a claim that arbitrary
FFmpeg buffering or an Android MediaCodec output index satisfies them.
The portable media, encoded-port and Iroh libraries check for Android ARM64.
Godot's separate workspace consumes the media configuration and Linux/Android
native decoders for a bounded AV1 fixture and a multi-window Iroh receiver
using the shared decode pool. Its playback and GLES/EGL presenter are shared,
with platform-selected decoder/native-buffer providers. See
[Godot native video](godot-native-video.md) and
[decoder reuse and follow-ups](receiver-decoder-pool.md#portable-execution-boundary).
The Godot receiver coordinator owns a `ClientRuntime` above its registration.
Desktop input enters a separate bounded mailbox, with displayed-frame geometry
and no native leases; the shared runtime owns route capture and adapter effects.
Godot objects stay on the main thread, where a typed Rust node owns event
extraction, physical-hold reconciliation and cursor presentation. GDScript only
wires the presented control; it does not own input policy or a cursor protocol.
See [Godot input](godot-hoisting.md#desktop-input).
One session owns the receiver, shared input mailbox and bounded surface
inventory. Rust projects that inventory into per-layer Godot views with
independent, generation-keyed GPU presenters; it does not create a transport
or decoder pool per window. Godot scenes currently arrange the window hierarchy
and native composition layers. Rust owns decoration and input-focus feedback.
The XR scene configures a separate native Rust pointer node with typed player,
controller and presentation references. It projects one right-hand aim ray
through the displayed quad into the same image rectangle and input mailbox;
it owns analog hysteresis, press retry/suppression and tracking/focus cleanup,
not a second transport or decoder. Generation/epoch tokens contain no native
leases. Only one desktop or XR input source may own a player. Godot retains
headset presentation and runtime tracking/model support; XR Tools is unused
and removed. See [XR input](godot-xr.md#right-hand-input).

`DecodeBackend::Output` and `DecodedFramePublisher::Buffer` bind the decoder to
a caller-owned publication adapter without making native buffers a protocol
type. The shared receiver retains decoded outputs until all layers of their
commit are ready. Only then does the publisher construct ordinary
`ClientBufferLease` values. Cancelled outputs drop without import. The publisher
also supplies its matching app-side client importer marker, so registrations
cannot independently choose an incompatible intake marker.

The Linux `DecodedDmabufPublisher` preserves core's external import, lease and
cache-removal path. Leases retain their own native state beyond publisher/port
lifetime. Destination-owned buffer/use IDs are allocated before publication;
a failed publication may consume IDs but never reuses them. A publication error
discards all results from that poll, including earlier complete commits, and the
destination relay disconnects, releasing the remaining unpublished outputs.
`EncodeBackend::prepare_input` similarly hides source access from scheduling:
the Linux backend copies SHM pixels or retains a DMA-BUF consumer until encoding
completes, without changing the existing source-lease release point.

`IrohNotifier` is a cloneable fallible wake callback, invoked after queue locks
are released. Linux adapts its existing eventfd notifier; other hosts supply
their own wake mechanism. This changes neither Iroh queue admission nor the
independent control/media tasks. Android checks establish compilation and the
absence of compositor dependencies, not network execution on Android. Separate
device probes and the Godot fixture validate native decoding/presentation. The
[Godot live viewer](godot-hoisting.md) uses the same registration and pool with
a native-image publisher; physical phone tests establish one live AV1 window
and source-restart reconnection, not XR runtime initialization.

FFmpeg and cros-libva intentionally own separate VA displays on the same render
node in this first implementation. Frames cross that boundary through PRIME.
Sharing FFmpeg's `AVVAAPIDeviceContext.display` with cros-libva could remove
that interop boundary later, but would couple two ownership models and is not
required for correctness.

The FFmpeg hardware-context integration is a substantial audited unsafe
boundary in `weld-media-vaapi::ffmpeg`, not a general escape hatch. It owns raw
AVBufferRef, AVFrame, AVPacket, codec, and filter-graph lifetimes; every unsafe
operation documents its local validity argument. The existing raw-libva VPP
submission remains isolated in `vpp`; replace it with cros-libva's safe API
when that crate exposes the missing typed pipeline-parameter buffer.

The local encoded binding carries raw H.264 access units or low-overhead AV1
OBUs in sealed descriptors on a media seqpacket channel separate from control.
AV1 is currently capped at the validated 8 Mbps operating point because a
64 Mbps radeonsi experiment reset the VCN context. H.264 defaults to 16 Mbps.
This is a backend safeguard, not an AV1 limit; see [VA-API workarounds](vaapi-workarounds.md).
There is no software fallback. Source DMA-BUF leases complete after hardware
encoding, while decoded DMA-BUF leases remain live through destination GPU
use. Decoder output allocation is not yet pooled, so the current path still
allocates and exports one destination surface per frame.

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
`HostBuilder` and `ApplicationHost` contract remain available for non-Bevy
hosts. `HostPolicy` supplies policy and `CompositionHost` supplies optional
rendering/capture, borrowed sequentially from that one owner. Backend module
entry points are implementation details.

Presentation-free assembly uses `weld-core::runtime::HostRuntime`.
The explicit `--backend headless` assembly bypasses `WeldApp`; it does not
instantiate a no-op renderer or an invisible nested window. Calloop, the
Wayland adapter, optional GPU import capability and a virtual output remain
alive without local presentation. All three assemblies use `NativeRuntime`'s
outer loop, display/client ownership, signal and readiness registration, ordered
client effects, child maintenance and independent presentation demand. Nested/DRM drivers retain
their native event ordering, local callback ledger, presentation deadlines, cursors and GPU submission.
Logical outputs remain separate from native attachment resources and availability.
Startup logical sizes configure once through native
constraints. Presenter claims drive per-root frame callbacks through one shared
scheduler, independent of local composition and buffer-consumer release. No
presenter means no periodic draw opportunities. SHM-only startup is valid without a
Vulkan adapter. `HostRuntime::with_policy` runs policy without drawing an output;
capture without a presenter fails explicitly. Adapter/wake registration is available to library consumers,
and the headless distribution installs a pending Iroh source before running it.
Authorization wakes the ordinary relay, whose constructor-time `AllToplevels`
policy admits existing and future mapped windows with one session per toplevel
and owner-session popups. Latest-state replay includes static windows; no
pre-admission media queue exists. Core has no hoist-specific lifecycle branch.
Reconnect and retained presentation preferences remain a separate follow-up.
See [Headless application hosting](headless-host.md).

`weld-app` represents application-visible outputs as `WeldOutput` entities.
`OutputGeometry` carries pixel size, logical scale and nominal presentation rate. `OutputPosition` locates
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
neutral `ClientSurfaceCommit` whose changed layer is retained, removed, or a
replacement `ClientBufferLease`. A lease contains adapter-private access and
completes only after its final consumer drops it. `AppShell` resolves every
lease before ECS ingress and asks the core-owned DMA-BUF manager to resolve a
Wayland or transported external DMA-BUF image into a Bevy handle. Application
plugins receive only retained content, pixels, or a Bevy `Handle<Image>` with
project-owned sampling metadata; they
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
resulting `CompositionHost` is a Bevy-free core contract. Each backend owns one
`ClientRuntime` above every registered adapter. It drains neutral events,
routes unconsumed device-paced input without a Bevy tick, applies neutral
requests and ordered adapter commands, advances application policy, and
requests composition into an owned or backend-leased target. The local
Smithay adapter shares reusable same-thread queues with `ServerState`, so
Smithay remains the concrete protocol dispatch state while input and requests
are applied in the same calloop turn. `AppShell` is the standard Bevy
implementation and owns the retained offscreen target, but core does not
require it.

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

Every `PendingWaylandDmabufUse` must either become a lease through
`DmabufContext::lease_dmabuf` or pass through `release_unrendered`. The
`DmabufAccess` moved into the resulting lease contains no Wayland release
identity. `DmabufManager` instead keys GPU consumption by the lease's neutral
`ClientBufferUseId`, allowing local Wayland and transported leases to share the
same renderer-completion path. The pending Wayland use does not yet have a
`Drop` fallback, so a future core-internal hardening pass may replace its
release duty with an RAII completion token. Violating this invariant retains
an explicit release point and its timeline import device, not only the legacy
buffer release.

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
picking, or plugins. An owned target remains necessary for capture, offscreen
composition, composed-desktop streaming, and composition while a physical
session is inactive. Presentation-free application hosting needs no such target.
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

`weld-client` owns bounded client cursor feedback: named shapes from the
standalone `cursor-icon` crate, hidden state, and validated straight-alpha RGBA
images with hotspots. Feedback is separate from surface commits and is selected
by `ClientRuntime`'s current pointer route/capture, including loopback aliases;
keyboard focus never selects a cursor. Unmap, destruction, and retired aliases
cannot leave feedback authoritative for an absent route.

`weld-core` owns backend-neutral cursor configuration, Smithay cursor-surface
lifecycle, and native raster normalization. Cursor callbacks only mark state
dirty; ownership is resolved after Smithay dispatch returns, outside its pointer
mutex. `weld-app` exposes reloadable
`CursorSettings`, interprets Bevy's standard `CursorIcon`, and accepts transient
`CursorRequest` overrides. Nested mode delegates final cursor presentation to
the host window system and supports custom Winit cursors. DRM mode normalizes
named and client cursor images into per-output Smithay `MemoryRenderBuffer`
elements. The global pointer is
projected into each output's local coordinates, so a cursor visual intersecting
a seam can be considered on both outputs. Smithay chooses each GBM cursor
plane; the existing composition blitter supplies the GPU fallback. Cursor-only
motion does not dirty the Bevy scene.

All hoist bindings carry cursor feedback over control, independently of codec
work. One cursor update may be outstanding per connection; subsequent updates
retain the newest desired image per surface until `CursorReceived` acknowledges
receipt, not display. Identical state is suppressed. Withdrawal does not free the
slot prematurely: the receiver acknowledges late feedback without resurrecting
the withdrawn surface. A missing acknowledgement stalls only cursor feedback,
with one warning after two seconds on the next ordinary relay poll.

The receiver retains the latest cursor preference across unmap/remap, but only
a currently mapped surface under the pointer has cursor authority. Newer cursor
control can overtake video-delayed mapping commits; erasing preference on unmap
would lose that update while source deduplication suppresses its resend.
Destruction and withdrawal still discard it. The source conservatively clears
its own remembered/sent cursor on observed unmap so a later enter can publish a
fresh default.

Bitmap cursors use destination-controlled scale-to-fit sizing: the larger
dimension equals configured nominal size times output scale, preserving aspect
and hotspot even when that requires upscaling a small sprite. This is shared by
local and hoisted cursors; clients choose artwork, not displayed nominal size.
The canonical source raster is at most 128x128 (64 KiB), with crop/viewport
normalization and no unconditional source upscaling. Native destination rasters
are bounded at 512 pixels per dimension. Explicit `CursorRequest` overrides
take priority, with the override flag change-tracked independently of the icon
so ending a resize restores client authority even when its hover icon matches.
DRM cursor selection runs outside the Bevy frame gate. See
[Cursor feedback](cursor-feedback.md) for validation and limitations.

Raw input is forwarded to the focused client in order and retained for the next
refresh-paced application update. Explicit keyboard repeats bypass the physical
input projection buffer; see [Keyboard repeats](keyboard-repeat.md) for stable
cadence ownership and the reloadable legacy fallback. This contract lets client
delivery run at device-event pace without running Bevy schedules at the device
polling rate.
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

`ClientSurfaceCommit::alpha_mode` separately records whether a media adapter
discarded the original tree's transparency. Ordinary Wayland commits preserve
alpha semantics, even for naturally opaque buffers. The opaque encoded source
declares `Discarded` before scheduling every commit, including retained-buffer
updates; native and loopback relays preserve that declaration. The encoded
destination rejects commits that do not declare the selected opaque mode.
`ClientBufferMetadata::opaque` and `MappedSurface::opaque` instead describe
pixel sampling and do not select a frame policy. The unreleased wire protocol
uses development revision 1; both endpoints must run matching builds. That
number alone is not a compatibility guarantee across development snapshots.

Encoded commits have no application ACK or per-surface stop-and-wait gate.
One encode batch runs at a time. Local media headroom admits the next batch;
pressure coalesces unencoded commits while ordered control and completed media
remain independently bounded. Transport write completion wakes static sources.
The receiver independently budgets control references and compressed bytes.
Its stream-affine decoder pool grows on actual work, up to four workers and
eight outstanding jobs (two per worker), with sixteen decoder generations shared
across the pool. Interaction-weighted group turns admit front commits and at most
one compatible successor per surface; age-based service prevents permanent
background starvation at available admission opportunities. Focus alone has
less weight than active input. See [interaction scheduling](interaction-scheduling.md)
for policy, ordering, and per-port scope. Workers submit available packets before waiting on older
frames; FFmpeg reserves extra frame slots for the explicit pipeline depth. Each
commit still applies atomically in FIFO order. Retirement protects
inventory, pending media, undecoded references and active jobs. Converted XRGB
output owns separate storage and does not keep an obsolete codec context alive.
Reservations remain charged until the worker acknowledges retirement.
See [receiver decoder pooling](receiver-decoder-pool.md) for lifecycle and limits.
Withdrawal preserves already-published media to resolve
late cancelled references. Native buffer release and cursor ACKs are unchanged.
See [ACK-free streaming](ack-free-streaming-plan.md) for bounds and limitations.

The default UI policy presents alpha-discarded toplevels with destination SSD
and a `WindowGeometry` content mount, retaining the source client's decoration
declaration. Cropping removes the client's shadow and invisible resize margins;
SSD restores resize and close affordances with a local frame and shadow.
In-geometry CSD controls, including Firefox's tab strip, remain visible beneath
the additional SSD header. Existing inset reconciliation preserves desired
client content size when the frame changes. Unmapping hides and retains the
existing frame; the next mapped commit resolves the current frame policy without
guessing during the gap. Cropping currently occurs during presentation; encoding
still carries the whole buffer. Pre-encode cropping requires a separate mapping
change because committed geometry can change even when buffer extent does not.

`WindowPresentationOverride` lets an optional presenter reserve one managed
window without teaching the default CSD or SSD plugins about that feature.
Those presenters revoke and suppress all of their primary, secondary, and
popup projections while an override is present. The owner may supply an
ordinary `PresentsWindow` tree or deliberately keep the window locally
unpresented while preserving its applied inset and outer-geometry contract.

Ordinary client policy follows direct `OccupiesWindow` relationships only.
`WindowClientResolver` consistently uses that occupant for presentation,
focus, configure/resize, close, output membership, preferred scale, and
protocol move/resize. `WindowAdmissionHold` lets another policy temporarily
keep a mapped client out of default admission without inventing a proxy
occupant or transport-specific branch in the window primitive.

`weld-hoist-core` implements the current same-process loopback as a real
`weld-client` adapter. The adapter observes local source events, retains their
current atomic role and commit state, and republishes selected surfaces under
an independent Relocated source namespace. Buffer replacements reuse the
upstream lease's erased access payload; completing the destination lease drops
one upstream consumer. Runtime route aliases rewrite destination input, focus,
close, resize, and output requests back to the source surface. Owner-related
popups are mapped automatically. Relay-generated events are published in the
same runtime drain but are not recursively observed, preventing relay cycles.

`HoistEndpointRegistry` is application orchestration, not a wire or transport
contract. Its monotonically allocated `HoistEndpointId` identifies one
registered endpoint only within the current Weld process; it is distinct from
a device identity, peer connection, presentation target, transport kind, and
`ClientSourceId`. The default endpoint is consulted only when starting a new
hoist family. Every resulting session stores that endpoint ID, so later family
members, reclaim, unmap, and failure recovery keep using the owning endpoint
even if the default changes. Active endpoints must allocate non-overlapping
destination `ClientSurfaceId` namespaces.

Registry entries remain present while sessions can reference them. A lost
connection is represented by `HoistEndpoint::is_available`, making the entry a
stable tombstone rather than allowing its ID to be reused for another peer.
The standard distribution currently registers one startup endpoint when it is
a hoist source or loopback host; destination-only startup installs neither the
registry nor `weld-hoist`. Dynamic endpoint registration, removal after the
last referencing session, and live multi-peer admission remain future runtime
work.

`Super+H` asks `weld-hoist` to detach and admission-hold every mapped toplevel
from the focused surface's stable `ClientId`. That identity represents one
adapter-namespaced Wayland client connection; it is not a process ID, app ID,
window class, or title. Same-client affinity covers independent application
windows that omit `xdg_toplevel.set_parent`, while explicit parent metadata
still describes the stronger toplevel-family relationship. Members already
present keep their durable source windows as hoist-owned Reclaim placeholders;
later same-client toplevels admitted through automatic following have no
source slot to preserve, while a direct request preserves the requesting
window's existing slot. The relocated toplevel enters `weld-window` through
ordinary admission and occupies an independent managed window. It therefore
uses normal CSD or SSD, focus, scaling, output membership, resize, CSD
interactions, and popup presentation. SSD's red hoist styling follows generic
Relocated provenance, not a hoist-specific window flag.

Loopback reclaim hides and configures the relocated receiver to the preserved
slot's client size, waits for settlement or a bounded recovery deadline, then
emits an ordered adapter `Unmap` command. The original client is reattached
only after the relocated surface's Destroyed event has removed the receiver.
Remote close leaves a preserved slot as a dismissible closed tombstone.
The loopback adapter remains useful as an in-process contract test. A separate
`weld-hoist-local` adapter now carries the same client lifecycle between two
sibling Weld processes. It does not turn the loopback representation into a
wire contract or add a media codec, network transport, pairing, or discovery.

The local binding uses Postcard records over an authenticated same-UID Unix
`SOCK_SEQPACKET` connection. `SCM_RIGHTS` attaches DMA-BUF plane descriptors to
the atomic commit that names them. The first use binds one allocation; later
uses refer to its stable buffer identity without duplicating descriptors or
reimporting the Vulkan image. Native `wl_buffer` destruction and final session
unmap send explicit retirement, while committed-use leases independently
return only after destination GPU consumption. This DMA-BUF path does not copy
pixels. A client that submits SHM instead uses a separate, explicit
compatibility path: core's already-normalized packed BGRA pixels are copied
into a sealed anonymous file, its descriptor crosses in the same commit, and
the destination validates and copies those pixels into its ordinary SHM lease.
SHM allocations are neither advertised as DMA-BUF nor entered into the
bind-once DMA-BUF cache. The send queue bounds descriptor ownership so a
stalled peer cannot retain an unbounded number of per-commit SHM files.

Encoded Unix sources, connected Iroh sources and pending Iroh sources all use
`EncodedSourcePort::configured` with the same transport-neutral options. Budget
attachment, access-unit dump setup and disconnect-on-configuration-failure live
in `weld-hoist-encoded`, not in each transport. Native Iroh assembly shares one
backend/wake constructor and source options type; destination namespace mapping
is an argument to the manual policy endpoint, not encoder configuration.
The pending wrapper owns authorization readiness only. Presentation claims are
separate from source construction: the source relay claims each mapped root,
and receiver output preferences supply its cadence. `ClientRuntime` validates
the claimant's declared upstream and routes its identity to the owning local
adapter. The Wayland bridge applies the claim in `ServerState`, where each
rate-less claim can be resolved against the root's preferred output before
choosing the fastest active consumer. Claimed roots bypass the native callback
ledger; a local display cannot delay their draw opportunities. Manual commands
route claims before native staging, and automatic admission routes them after
ordinary ingress. Local roots still use actual native presentation completion.

The mental model is moving a window between monitors: size, scale and cadence
follow the presenter. Foreign output IDs do not become local native outputs.
No per-frame acknowledgement is added, and callback completion never releases
a GPU lease. This boundary correction alone does not diagnose the observed
headless latency or AV1 VCN resets.

Destination requests and already-addressed input re-enter `ClientRuntime`
immediately after transport ingress, outside Bevy's paced frame gate. Foreign
output IDs never reach the source. Instead, destination output membership
publishes a fractional `scale_120` preference that temporarily overrides the
source surface tree's preferred scale while leaving its real output
enter/leave state unchanged. Reclaim configures the authoritative source
surface to its preserved placeholder size and waits for a newer source commit.
Peer loss synthesizes exact releases for remotely held keys, buttons, gestures,
and finger scrolling before restoring source presentation.

The first supported topology is two sibling compositor processes. A source
blocks for one startup peer and a destination connects before either runtime
starts; dynamic admission and reconnect are not implemented. The Unix binding
uses a same-UID local socket. The encoded Iroh binding uses an authenticated,
encrypted connection reached through a trusted source ticket and checks the
destination's authenticated EndpointId against an explicitly supplied identity.
The binding owns private one-shot rendezvous files and bounded concurrent
bootstrap; neither enters the semantic hoist protocol. This is transport-peer
approval, not device pairing or mesh authorization. Nesting the
destination as a client of the source is deliberately unsupported because it
would create an input/focus feedback path. See [Local hoisting](local-hoisting.md)
and [Iroh hoisting](iroh-hoisting.md) for commands, validation, and current
constraints.

Core translates Smithay's `xdg_toplevel.set_parent` state into stable
`SurfaceId` parent metadata; no Wayland object crosses into the application
model. `WindowFamilyResolver` follows direct authoritative occupants through
that metadata. The local hoist plugin groups one source/destination session
per independent toplevel under one same-client family ID and reclaims every
admitted toplevel in that group atomically. Members present when hoisting
begins retain their source layout slots and receive individual Reclaim
placeholders. Members first admitted through automatic following while the
client group is already active are receiver-only because they never occupied
pre-hoist source layout. Reclaiming or unmapping members temporarily block
further admission for that client. Closed tombstones retain their original
layout slots but do not block admission or keep a client group active. A newly
opened same-client replacement may therefore be followed without adopting or
removing the old tombstone's slot. If no live session remains, a direct request
may start a fresh family while old tombstones remain independently dismissible.
If another member keeps the group active, locally opened windows are followed
after transition blocking settles, except members explicitly detached from
that family by reparenting, protocol unmap, or receiver loss; ending the final
member ends the group instead. A direct `Super+H` request opts a detached member
back into an unblocked active group while preserving its local slot. A later
family may also admit it normally because detach state is scoped to one
`HoistFamilyId`. Destroying a captured member remotely keeps its slot as a
dismissible closed tombstone without a Reclaim action; ordinary protocol unmap
instead ends that member session so a later remap can return through default
presentation. Popups and subsurfaces remain within their owning surface tree.
An independent same-client peer that later joins the declared parent tree is
promoted to an explicit family member. If that member is subsequently
re-parented out, it alone is unmapped from the destination and restored
locally and remains detached from automatic following for the lifetime of that
family; the remaining client group stays hoisted. SSD uses focused and
unfocused red border shades from generic Relocated provenance without querying
hoist-owned state.

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

The shared hoist relay forwards every mapped popup role update, including
position and stack changes, and replays pending popup roles when their owner
mapping arrives. Popups remain independently streamed children of the outer
window presentation, outside its content clip. Alpha-preserving popups show
their complete surface at the owner anchor plus popup position and visual
offset. Alpha-discarded popups instead mount `WindowGeometry` at the owner
anchor plus popup position, clipping only their own mount through Bevy's UI
overflow and picking. Mount view and clipping update in place when alpha mode
changes. Input retains the original surface-local coordinates through the
existing `SurfaceNode` geometry origin mapping.

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
