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
- physical pixel extent and density or scale;
- orientation, refresh range, and presentation cadence;
- color, transfer, HDR, and alpha capabilities;
- destination decoration and visual-overflow policy;
- pointer, touch, keyboard, IME, gamepad, and accessibility input;
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
