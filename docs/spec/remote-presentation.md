# Remote presentation targets and quality

## Boundary and status — Direction

This document describes how a destination viewport becomes a remote
presentation target and how Weld communicates geometry, scale, coded extent,
and expected quality. The
[remote protocol](remote-protocol.md) owns handshake, capability negotiation,
transport, surface modes, codec profiles, and media stream layout. The
[hoisting model](remote-hoisting.md) owns window-family lifecycle, admission,
reclaim, and source authority.

A device is not a presentation target. One phone may expose its main screen,
picture-in-picture, a foldable region, and an external monitor as different
targets with independent policies.

## Presentation target model — Direction

A target advertises stable identity and current observations including:

- layout mode and maximum simultaneously visible presentations;
- usable logical viewport and safe-area insets;
- optional spatial placement, view-frustum, and attention observations;
- physical pixel extent and density or scale;
- orientation, refresh range, and presentation cadence;
- color, transfer, HDR, and alpha capabilities;
- destination decoration and visual-overflow policy;
- pointer, touch, keyboard, IME, gamepad, tracked-hand, controller, and
  accessibility input;
- power, thermal, data, latency, and quality preferences; and
- whether resizing, letterboxing, destination scaling, or cropping is allowed.

Target observations do not copy a native monitor object to the source. The
source validates target preferences and projects accepted logical size, scale,
color, and cadence into its own Wayland and window policy. Client lifetime,
configure sequencing, committed buffers, and accepted results remain
source-authoritative.

Capabilities describe what a target can do. Dynamic observations describe its
current viewport, battery, thermal, orientation, and visibility. A target can
update observations without repeating device authentication or unrelated
codec capabilities.

Only observations required by the authorized session and selected adaptation
policy are reported. Battery, thermal, viewport, orientation, and visibility
observations are not persisted or forwarded by default. A destination may
coarsen or disable them; the source then uses conservative adaptation rather
than treating absent telemetry as permission to infer more.

A destination layout may derive target-local active-workspace presence,
visible fraction, and full or partial occlusion from trusted presentation
geometry, z-order, and visibility. These are destination observations, not
Wayland-client assertions or window-management primitives, and follow the same
session-scoped disclosure and optionality rules. Missing or coarse observations
cannot justify treating a presentation as hidden.

Destination-local touch-to-pointer translation is not native touch delivery.
True touch, text composition, selection, and IME behavior require explicit
source and destination capabilities. Shell-owned navigation gestures remain at
the destination and are not forwarded implicitly.

## Target layout modes — Direction

A **single-active** target, such as a phone, presents one primary toplevel at a
time. Other admitted windows retain identities and lifecycle but may pause
video or publish low-cadence thumbnails. A destination task switcher changes
the active presentation without reclaiming or remapping its source window.

Related dialogs and popups remain distinct protocol roles. A phone shell may
place a related dialog over the active presentation or dedicate the viewport
to it. Popups remain positioned relative to their owner and cannot become
unrelated freely managed windows merely to fit the mobile UI.

A **freeform** target presents independent windows in a desktop-like canvas. A
**workspace** target mirrors or melds several logical workspaces. Foldables and
external displays may expose several targets or one target whose viewport
changes; that choice is destination policy. Every mode preserves stable window,
surface, relationship, and input identities.

A target may also expose a **sliding set** or a conceptually unbounded canvas.
These are presentation policies over an admitted collection, not protocol
surface modes. The protocol supplies stable windows, relationships, ordering
hints, and lifecycle; the destination decides whether they appear as a strip,
carousel, layered spatial field, or infinite scrollable canvas. Changing that
layout does not remap, reclaim, or change the identity of its windows.

## Spatial and XR targets — Exploration

An XR headset is a destination presentation adapter, not a new client-surface
mode or transport. Each hoisted window remains an independent presentation that
the destination may place, scale, curve, hide, or group in a spatial canvas.
Popups and related windows retain their protocol identities and relationships;
the headset may present them as attached planes or promote them into suitable
spatial panels without flattening the application into one remote desktop.

When an XR target requests a window, workspace, or desktop handoff, it may mark
stereo as desired or required for each presentation. Source policy projects that
request through the application's optional
[view-set contract](surfaces-and-input.md#application-provided-view-sets--exploration).
The request can therefore toggle a cooperating window between mono and stereo,
but cannot manufacture a second view for an ordinary application. The target
continues presenting the last accepted configuration until the source reports
the acknowledged replacement commit.

The headset can derive visible fraction, occlusion, distance, projected pixel
extent, and whether a presentation intersects the current view frustum. These
are target-local observations under the same trust and optional-disclosure rules
as ordinary destination visibility. Head pose changes spatial composition at
the destination and do not become application pointer motion.

Eye tracking is optional, not implied by an XR target. Head/controller-directed
regions can prototype enhancement placement without being treated as eye-gaze
measurements.

Readability requires separate controls for panel angular size (physical size and
distance), client logical/UI scale, and encoded pixel extent/quality. A high
resolution stream on a visually tiny panel can still have unreadable text;
enlarging a low-resolution texture cannot restore missing detail. Use projected
pixel demand and user preferences to request an appropriate client scale and
stream quality. An optional reading mode may enlarge or reposition a panel, but
avoid involuntary movement or application relayout on every focus change.

Eye tracking introduces an **attention** observation, which is distinct from
application input and keyboard focus. By default, raw gaze coordinates remain
on the headset. The destination may instead report a coarse presentation ID,
attention strength, and recency so media budgeting can raise quality for the
gazed-at window and reduce cadence, extent, or bitrate for peripheral or unseen
windows. Dwell thresholds, hysteresis, and a short grace period prevent small
eye movements from continuously renegotiating streams. Region-level foveated
quality inside one window is a later media-layout capability, not a prerequisite
for per-window prioritization.

These form two nested allocation levels. A spatial workspace may keep every
visible background presentation available as a low-resolution or low-cadence
base. Focus and gaze first promote one presentation to the interactive quality
tier; only that presentation normally receives a higher whole-window bitrate
and gaze-local high-resolution enhancement. Moving attention transfers that
enhancement budget rather than maintaining full-resolution encodes for every
open application. A short grace period may retain the previous target long
enough to avoid a visible quality flash during accidental gaze crossings.

For remotely presented content, **foveated streaming** may add a continuously
decodable low-detail base image covering each complete eye view plus a
high-detail enhancement region centered around predicted gaze.
[Valve has described its Steam Frame game-streaming
path](https://www.pcgamer.com/hardware/vr-hardware/foveated-streaming-genius-tech/)
in this form: two low-resolution full views and two high-resolution gaze-local
fragments. Weld should treat that as strong prior art for a general media
layout, not as a dependency on Valve's wire format or implementation.

The destination retains raw eye samples and projects only the smallest useful
request into normalized presentation or eye-view coordinates: a sample time,
prediction time, region center and extent, and optional confidence. Requests
are latest-value observations; stale gaze must be discarded rather than queued.
A guard band around the fovea, overlap or feathering at the boundary, and a
stable low-detail base hide tracking, network, and decode latency. Loss or late
arrival of the enhancement region degrades local sharpness for that frame but
does not freeze or invalidate the base presentation.

This is separate from application foveated rendering. Streaming foveation can
be application-agnostic because it redistributes encoded detail after the full
image exists. Across a workspace it can substantially reduce source-side
scaling, composition, color conversion, encoding, and network work because most
presentations never enter the full-resolution media path. By itself it does not
reduce the source application's own rendering cost if that application still
produces a full-resolution client buffer. Weld may separately request a smaller
source raster, and a cooperating application may render peripheral pixels more
cheaply, but both use application-facing contracts and remain independently
negotiated.

Spatial attention, pointer or gesture targeting, keyboard focus, and permission
to inject input are separate states:

- gaze may prime a presentation for higher quality without focusing its client;
- a controller action, hand interaction, configurable gaze dwell, or explicit
  shell action may request focus for a stable presentation identity;
- the source validates that request against the authorized session and the
  core [logical-seat
  contract](surfaces-and-input.md#seats-and-devices--direction) before changing
  authoritative client focus; and
- an input device sends events only through the seat and focus route to which it
  is assigned.

Source-attached laptop input and headset-attached input follow the shared
[input-producer and remote-control contract](surfaces-and-input.md#input-producers-and-remote-control).
The XR shell selects presentations and expresses focus intent; seat ownership,
held input, repeat and revocation are not XR-specific implementations.

An XR target may use a focused-window-relative pointer instead of projecting a
mouse across one global desktop rectangle. The target negotiates whether a seat
uses surface-local absolute coordinates, ray intersections projected into a
surface, or relative deltas captured by the focused presentation, under the
[pointer-capture contract](surfaces-and-input.md#pointer-capture-and-relative-motion--direction).
The destination owns cursor
placement in its spatial scene; the source still supplies client cursor shape
and hotspot changes and validates input before delivery.

### Initial ring workspace

The initial layout candidate is a circular arrangement around the user, starting
with a front-facing arc and stable slots rather than unrestricted free-form
placement. Use a consistent viewing distance, face each panel toward the
workspace's reference position, and allow a modest adjustable backward tilt
(top edge farther away, like a laptop screen). Related menus, tooltips and
dialogs remain attached to their application family rather than receiving
unrelated ring slots. Independent placement can come later.

Two session-local anchor modes are useful:

- **Pinned:** the workspace stays at a chosen room position/orientation.
- **Follow:** recenter around the user as they move, without rotating on every
  head turn and making a looked-at window move away. The precise follow threshold
  and smoothing need comfort testing. Freeze automatic relocation during an
  interaction and provide an explicit recenter action.

Persistent anchors across restarts, room changes and tracking relocalization
are separate work. Neither mode rotates the camera or the perceived room to
navigate applications. Head tracking and window layout remain separate.

Ordinary physical-mouse motion uses a window-local cursor constrained to the
focused application, with its previous position restored on return. This is
not application-requested relative capture: Blender's hidden-cursor value drag
still needs unbounded relative deltas and proper capture activation.

For ring navigation, try holding a selected mouse side button and moving the
mouse horizontally. Capture/hide the pointer, rotate the ring, and highlight a
candidate without changing accepted application focus. Release requests focus
and gently snaps that panel into place; Escape cancels to the previous choice.
Start without inertia. Do not enter during an application drag/grab. Restore
the selected window's cursor only after the transition is accepted.

The binding is configurable and reserved only while session input capture is
active. Detect the actual extra-button event; do not confuse Godot button
numbers with native input codes. Consume both edges so the gesture cannot also
send browser Back/Forward. Ctrl+Alt is a possible keyboard fallback, not a
conflict-free guarantee. Alt alone conflicts with applications; Super may be
owned by Sway and is a possible default only when the host configuration or
Weld's full-stack ownership permits it. Head/controller pointing plus explicit
confirmation and conventional task cycling remain alternative selection modes.

### Controller interaction and virtual keyboards

Start with controller-ray intersections mapped to application-local mouse
coordinates: trigger for left press/drag/release, another button for right
click, stick for scrolling, and configurable middle-button/modifier bindings.
Mouse semantics preserve desktop hover and menus. Finger interaction may later
use true touch when the end-to-end contact contract is supported; translating
a tap into a mouse click must not advertise native touch support.

A controller button explicitly toggles the XR keyboard initially. Automatic
opening on application text-field focus is not required. Showing or pointing
at the keyboard must preserve the intended application's keyboard target;
closing it releases only its own held/latched inputs. XR Tools keyboard
components could be reintroduced as a UI candidate without restoring the whole
addon; they are not a ready-made remote keyboard adapter:
their synthetic key events require balanced release, layout/modifier and
repeat handling at Weld's input boundary.

Phone on-screen controls and Android's native keyboard are later interaction
work. Native keyboard availability does not supply remote IME composition,
selection or text commit support; those follow the shared input capabilities,
not guessed physical-key sequences.

### Hover haptics and target assistance — Exploration

Explore optional controller assistance for small desktop controls in XR:

- A short haptic pulse on entering a new known actionable control, with a
  cooldown and target identity so boundary jitter does not repeatedly buzz.
- Gentle attraction toward an eligible button or tab, plus hysteresis: use a
  larger leave boundary than enter boundary to resist hand wobble. Deliberate
  movement away must readily disengage assistance; never synthesize activation.
- Keep raw aim distinct from assisted application-pointer coordinates, and
  show feedback at the effective target so the visual does not mislead the user.
  Do not apply attraction to free viewport motion, sliders, resize handles or
  active drags. Assistance must not fight pointer capture or precision work.

Cursor shape alone is insufficient: Blender can retain an arrow over buttons,
and a text cursor does not establish editability. Investigate host-side
accessibility hit testing (for example AT-SPI) for target role, bounds, state
and identity. Blender's actual accessibility coverage remains unverified.
Associating an accessibility object with a specific hosted surface and mapping
its coordinates through crop/scale are prerequisites, not assumed capabilities.

Any semantic feedback should be optional, scoped to the authorized window and
queried asynchronously with bounded work. Never wait for an accessibility or
network round trip before forwarding ordinary input. If target metadata is
missing, stale or inconsistent with the current geometry, fall back to raw
pointer behavior. Do not transmit text values or a whole accessibility tree
merely to identify a button. User controls should include disabling assistance
and tuning haptic strength and attraction.

An earlier experiment could use mild, low-latency aim stabilization and a small
pulse when the local input path accepts a trigger press. That pulse must not
claim the remote application accepted or completed an action. Neither this
experiment nor semantic assistance is implemented by the current pointer.

### Initial headset readability preference

The vendor-reported [Pico 4 Ultra panel resolution](https://www.picoxr.com/uk/products/pico4-ultra)
is 2160x2160 per eye, not a measured per-window raster budget. Headset render
targets, lens projection, panel angular size and distance determine the useful
window sampling density. Keep UI scale, encoded extent and apparent panel size
independent; ordinary mono windows can be sampled into both eye views without
requiring two separately encoded application images.

Try an adjustable **1.8x application UI-scale preference**. For example, a
2160x1440 source raster at that scale represents about 1200x800 logical units;
do not force square windows just because the eye panels are square. This is an
experiment, not a promise that every client honors an exact fractional scale,
nor a requirement to encode every visible window near 2160 pixels.

The current [Godot tracer](../godot-hoisting.md#pipeline-and-limits) rejects
extents above 2048 per dimension or 1920x1080 total pixels. The example exceeds
both deliberate admission limits; it requires decoder/capability/budget
validation before raising them. They are not probed Pico or AV1 hardware limits.

### Physical-monitor overlay and window detachment

A much later presentation mode could replace the blurry passthrough image of
a laptop screen or desktop monitor with a sharp digital workspace view aligned
to the physical screen. The user begins at the familiar workstation, grabs an
existing window out into XR space, then returns it when finished. This changes
presentation ownership, not the identity or running instance of the application.
Normal hoist/reclaim policy governs the transition and any source placeholder.

A desktop video alone does not expose detachable application surfaces. The
source also needs authorized stable window/output identities, geometry and
relationships, provided by Weld hosting/proxy integration or another cooperating
source. Screen detection aligns the physical plane; it does not infer permission
or reconstruct a reliable client graph from rectangles in video. An overview
stream versus destination composition of window streams is a later budgeting
choice, not a new semantic hoist mode.

Start with manually aligned and pinned screen corners. Later, an on-screen
marker could identify/calibrate an output without bypassing pairing/consent.
Automatic detection/tracking may eventually use OpenCV or similar libraries
from Rust with camera feeds, under the
[platform constraints](distributions.md#godotrust-phone-first-xr-client).
Validate camera access/permissions, intrinsics, camera-to-headset extrinsics,
timestamps, reference-space registration, latency and tracking-loss behavior;
detecting a rectangle alone does not provide a stable XR pose. Hands/objects
occluding the screen and alignment drift need independent treatment. Keep raw
camera frames and room observations local unless explicitly authorized otherwise.
This is screen perception layered on normal XR pose tracking, not a requirement
to rebuild headset tracking, and is not a prerequisite for the first shell.

### Fixed and head-tracked spatial content

Fixed stereo and head-tracked stereo have different lifecycle and sharing
requirements. Fixed stereo content, such as a stereoscopic film or an emulator
with stable left and right images, declares a view relationship but needs no
headset pose. One synchronized rendition may therefore be shared by compatible
viewers. A head-coupled Blender viewport renders from the viewer's predicted eye
poses and generally needs a separate rendition for each independently moving
viewer.

A tracked target issues a session-scoped view request containing a reference
space identity, predicted display time, sample or generation identity, per-eye
pose and field of view, recommended raster extent, and the transform from the
application scene into that reference space. The application result names the
request it rendered, the actual poses and fields of view, and the rectangles
containing each view. Room-scale coordinates remain local to the XR session;
Weld exchanges relative spaces and transforms unless the user explicitly
authorizes a broader spatial map.

Network delay makes the source-rendered pose historical by presentation time.
The destination may late-reproject the result using current tracking. Optional
depth improves translational reprojection and spatial occlusion, but it cannot
reconstruct pixels that were never visible to the source view. Prediction,
deadline handling, stale-frame dropping, and reprojection metadata are part of
the presentation contract rather than pointer input.

The protocol-neutral spatial result is conceptually a frame group containing:

- a synchronized color view set;
- optional alpha and depth view sets;
- the acknowledged view-request identity and rendered pose metadata;
- content bounds; and
- an explicit input region.

All planes share one frame-group identity even when media negotiation carries
them in separate payloads or at different resolutions and cadences. Packed eye
views can remain one encoded access unit. Alpha may use an auxiliary payload
when the selected color codec has no alpha, while depth may use a lower-rate or
lower-resolution representation. Each plane consumes an explicit resource
budget. Transparency never implicitly defines hit testing: a transparent
spatial surface still needs an input region and destination input policy.

A foveated enhancement is another synchronized region in this frame group, not
a new logical surface. Its destination rectangle, source crop, eye association,
quality role, and gaze-sample identity are explicit so compositing cannot place
an old sharp region over a new base frame. Implementations may carry base and
enhancement as separate streams, codec layers, tiles, or a codec-native region-
of-interest map. That choice remains inside negotiated media capabilities.

This produces a useful progression without claiming a geometry protocol:
ordinary mono windows, mono cutouts with alpha, fixed binocular cutouts,
head-tracked binocular content, and depth-assisted spatial content. These are
raster views with optional reprojection data, not a substitute for meshes or a
complete scene graph. Local applications and transported applications expose
the same result semantics; only the latter require codec and network framing.

## Window geometry and visual overflow — Direction

A usable client-declared xdg window geometry is the preferred visible-content
baseline. The source intersects it with available root content; a degenerate or
fully outside rectangle does not authorize trimming. When geometry is unset or
unusable, the intended safe remote policy retains the complete surface-tree
bounds rather than guessing where client content ends and shadow begins.

Current Weld carries an optional `SurfaceWindowGeometry` and applies a clamped
crop to the root view only. Unset, degenerate, or fully outside geometry leaves
that root view uncropped, while subsurface overlays are collected separately
without the geometry crop. Weld therefore does not yet compute complete
surface-tree visual overflow. The remote presentation implementation must close
that gap before promising automatic shadow trimming for every client.

The current opaque encoded path declares discarded alpha on its commits. The
default destination presenter uses SSD and clips to declared window geometry,
while preserving the client's original decoration preference and in-geometry
controls. Popups use their own geometry crop outside the owner's content clip.
This is presentation cropping; the encoder still receives complete buffers.

Client-side decoration is not visual overflow. Firefox tabs, titlebar buttons,
menus, and other CSD controls inside the declared window geometry remain part
of the presentation. Weld cannot infer a generic application content box and
must not crop CSD controls merely because a small destination supplies its own
shell chrome.

Pixels outside window geometry are optional **visual overflow**. A constrained
target may trim client shadows and render a destination-owned shadow. A
freeform alpha-capable target may request the overflow. The offer records
whether overflow is retained, trimmed, or unsupported; it is never confused
with the window's input or layout geometry.

Server-side decoration and compositor effects are destination presentation,
not client media. A destination can render its own frame, shadow, focus state,
placeholder, or hoist indicator without spending codec bandwidth or changing
the authoritative client surface.

## Extent and performance envelopes — Direction

The presentation contract tracks distinct extents:

- **Logical window extent** controls application layout and configure state.
- **Source raster extent** is the pixel buffer actually rendered by the
  Wayland client after its chosen scale and viewport behavior.
- **Encoder-input extent** is the cropped, composed, or converted GPU image
  submitted to the encoder.
- **Coded extent** includes codec-required alignment and padding.
- **Visible stream extent** excludes coded padding and defines carried detail.
- **Destination extent** is the physical region receiving the decoded image.

These values must not be collapsed into one width and height. A codec or
hardware implementation may have hard dimension and alignment limits while
also supporting different extent-and-cadence performance points. Aggregate
pixel rate and concurrent session count may constrain several presentations
before an individual maximum dimension does.

A window can remain logically larger than the selected codec extent when Weld
is permitted to resize, downscale, tile, or present only a viewport. Therefore
codec extent is not universally the maximum window size. It becomes a hard
window limit only when the user requires unscaled complete pixels and no
compatible tiling or higher profile exists.

## Scale and raster strategies — Direction

The source and destination explicitly select among these strategies:

1. **Reflow to fit** configures a smaller logical window so the application
   lays itself out within the stream performance point. It carries crisp pixels
   but may expose less content or change responsive layout.
2. **GPU downscale** preserves the larger logical layout and source raster,
   then crops or downsamples into the encoder extent. It preserves layout but
   reduces transmitted detail.
3. **Source supersampling** projects a higher preferred client scale before GPU
   downsampling into the same coded extent. It may improve antialiasing and edge
   stability at additional client and source-GPU cost, but cannot transmit
   detail beyond the coded extent.
4. **Destination upscale** decodes the selected visible extent and enlarges it
   to the target. It is inexpensive with ordinary filters but does not restore
   detail that was never transmitted.
5. **Exact or tiled** refuses downscale and either selects a larger compatible
   profile or divides the presentation into negotiated tiles. Tiling increases
   session, synchronization, mapping, and recovery complexity and is not an
   initial requirement.

Wayland scale is a preference and protocol interaction, not a guarantee that a
client renders the exact requested raster. Weld may project a destination's
scale and output characteristics, but the client controls its submitted buffer
and supported fractional-scaling behavior. The source measures the committed
raster and derives the actual quality assessment from it.

A high-DPI target therefore needs an explicit policy. For a 4K target and a
1080p stream envelope, the user may choose reflow at a smaller logical size,
preserve the 4K layout and downscale, enable source supersampling, accept
destination upscale, or require an exact higher-resolution or tiled path. Weld
must not silently advertise 4K presentation as 4K transmitted detail.

## User-visible quality contract — Direction

Technical capability selection is translated into a concise quality
assessment. At minimum, UI can show:

- selected codec and source/destination hardware or software stages;
- logical, source-raster, visible-stream, and destination extents;
- stream-to-target scale factor and cadence;
- the largest current presentation that retains one-to-one stream detail;
- whether layout is reflowed, pixels are downscaled, or output is upscaled;
- supersampling and its source load without claiming extra transmitted detail;
- alpha, HDR, color, and visual-overflow degradation; and
- the constraint responsible: codec limit, decoder performance, session
  budget, bandwidth policy, power policy, or user override.

Example messages include:

```text
Sharp up to 1920x1080 at 60 Hz. Larger windows are downscaled.
Displayed at 3840x2160 from a 1920x1080 stream (2x upscale).
Source renders at 3840x2160 and downsamples to 1920x1080.
Exact 4K is unavailable: the destination supports 4K only at 30 Hz.
Software AV1 saves bandwidth but may increase battery use and heat.
```

“Maximum window size” is used only for a genuine hard policy. In the common
case, “native-detail threshold” or “clarity decreases above this size” is more
accurate. The wallet or destination may remember a user's quality policy per
device, target, application, or network class while still showing the active
result.

Reported capacity, per-window priority, quality tier, and the reason for a
reduction or suspension come from
[Remote media budgeting](remote-budgeting.md). Presentation owns how those
facts are explained, not the scheduler decision.

## Spatial upscaling — Exploration

Ordinary nearest, bilinear, bicubic, Lanczos, sharpening, and edge-adaptive
spatial filters can operate on one decoded frame without client cooperation.
An
[FSR 1-class spatial path](https://gpuopen.com/fidelityfx-superresolution/)
is therefore technically plausible in a wgpu or platform adapter. It still
needs evaluation on text, thin UI lines, subpixel antialiasing, transparency,
latency, power, and repeated source-downscale/destination-upscale chains.

Temporal game upscalers are not generic post-processing filters. Techniques
such as [FidelityFX temporal super
resolution](https://gpuopen.com/fidelityfx-superresolution-2/) and
[DLSS Super Resolution](https://developer.nvidia.com/rtx/dlss) normally consume
engine-provided motion vectors, depth, jitter, exposure, and related history
that an arbitrary Wayland surface does not expose. They are unsuitable as a
baseline unless a cooperating application or protocol extension supplies the
required data.

Platform video-super-resolution facilities such as the
[NVIDIA RTX Video SDK](https://developer.nvidia.com/rtx-video-sdk) may provide
optional decoded-video enhancement on compatible hardware. They remain
vendor-specific destination adapters and must report added latency, power,
supported formats, and whether UI or alpha content is safe. They do not change
the negotiated visible-stream extent.

## Progressive detail and screen-content techniques — Exploration

Remote application UI differs from camera video. Useful experiments include:

- screen-content codec tools where hardware exposes them;
- damage- and edge-aware region-of-interest quality allocation;
- cursor, focus, text, and active-control prioritization;
- dynamic stream resolution during motion or congestion;
- a low-latency base video plus high-resolution static refinement tiles;
- cached destination thumbnails for paused single-active presentations; and
- destination-owned decoration, shadows, cursor, and shell UI.

Static refinement is particularly promising for large desktop UI on a
codec-limited target. Once damage settles, the source can transmit
high-resolution tiles for text and static regions. The destination overlays
them on the base video and invalidates only tiles intersecting later damage.
This may provide sharp 4K static content without requiring a continuous 4K60
video path, but it needs its own bounded payload, cache, synchronization,
color, alpha, and failure semantics.

These techniques are not advertised until measured. A quality enhancement
must be removable without changing window, input, hoist, transport, or codec
identity.

## Initial presentation tracer — Exploration

The first encoded local tracer uses one single-active target, one presentation
and region, and a selected extent that fits one real hardware performance
point. For a client with usable window geometry, it trims only overflow
excluded by the root crop. Absent or unusable geometry and subsurface overflow
remain untrimmed validation cases. The tracer reports all six extents and
whether any source or destination scaling occurred.

Validation compares native-buffer and encoded modes for Firefox CSD, popup and
dialog placement, input coordinates, destination-driven configure and scale,
reclaim, and clarity at one-to-one and upscaled target sizes. No enhancement
filter, atlas, tiling, or refinement layer is required for the first picture.

Hardware-constraint calibration and concurrency coverage are tracked by the
[budgeting measurement
matrix](remote-budgeting.md#initial-measurement-matrix--exploration).

## Open work — Exploration

- Define the exact presentation-target and quality-assessment records.
- Extract complete surface-tree content bounds and optional visual overflow,
  including subsurfaces and unusable or absent xdg window geometry.
- Measure source reflow, downscale, supersampling, and destination upscale on
  text-heavy, video, game, and alpha content.
- Validate spatial filters and platform video enhancement on UI quality and
  battery rather than assuming game or camera-video results transfer.
- Prototype damage-invalidated static refinement without delaying input or
  the low-latency base stream.
- Decide when exact oversized presentation justifies tiling rather than an
  explicit quality warning or source resize.
- Prototype an XR spatial target with independent windows, destination-led seat
  focus, source- and destination-attached keyboards, and privacy-preserving
  gaze-derived attention.
- Validate one fixed-stereo application and one head-tracked application against
  the same view-set contract, including pose prediction and stale-frame policy.
- Validate synchronized color, alpha, and optional depth planes without deriving
  input regions from pixel transparency.
- Prototype a foveated base-plus-enhancement layout, measure gaze-to-photon
  latency and boundary visibility, and compare it with codec-native ROI maps.
