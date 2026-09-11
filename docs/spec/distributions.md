# Distributions

## Current assembly — Implemented

Weld is already a workspace of reusable crates: `weld-core`, `weld-app`, and
the optional `weld-window` policy feed the standard `weldwm` distribution. A
consumer can assemble a different Bevy application and plugin set around the
same host boundaries. See [Architecture](../architecture.md).

## Distribution strategy — Direction

Weld should make it practical to “weld together” a personal operating
environment from libraries and plugins. Shared crates own mechanisms and
stable extension surfaces; distributions choose defaults, configuration,
shell UI, layout, networking, and enabled native capabilities.

Do not create forecast-only crates such as `weld-network` or `weld-sandbox`
until an implemented responsibility needs an independent dependency, runtime,
reuse, or testing boundary. Feature names and package names remain provisional
until then.

## Proposed starter distributions — Exploration

### Gaming

A handheld and gaming-oriented assembly could combine the default compositor,
the [gaming sandbox](gaming-sandbox.md), controller-first shell UI, local game
launching, and low-latency [remote hoisting](remote-hoisting.md). Its defaults
would favor predictable virtual modes, hardware media, gamepad ownership, and
fullscreen presentation.

### Workspace/server

A headless-capable assembly could retain Wayland clients without requiring an
active physical output and export selected application windows to thin clients.
Local administration, authentication, resource limits, recovery, and explicit
stream ownership matter more than a full local shell.

### Mobile client

A phone and tablet assembly could combine the
[identity
wallet](identity-and-meshes.md#device-wallet-and-pairing--exploration),
remote application discovery, and one or more destination presentations. The
current first experiment is the Godot/Rust phone-first client below. The earlier
native-shell candidate remains
[the Dioxus project](https://github.com/DioxusLabs/dioxus) with its Native
renderer and [Blitz](https://github.com/DioxusLabs/blitz). Dioxus Native remains
an experimental candidate rather than a protocol or library boundary.

The experimental [iroh-live](https://github.com/n0-computer/iroh-live)
workspace's Dioxus and Android adapters are evidence that Dioxus WGPU
presentation and Android hardware-buffer presentation are separately feasible.
Weld should validate their integration rather than assume they already provide
one unified path.

The assembly should hide its UI and media integration behind a
destination-owned view that presents one remote window's decoded media and
forwards input against stable protocol identities. A custom WGPU paint source
is a candidate for a later Blitz experiment. A native Android EGL surface
embedded in the shell remains a valid hardware path, and the Dioxus web renderer is an
acceptable fallback if it can present encoded video without per-frame
raw-pixel CPU readback. Changing among those paths must not affect pairing,
mesh authorization, hoist lifecycle, or the wire contract.

Phone layout should not imitate a desktop indiscriminately. A small display may
present one remote toplevel at a time with a task switcher while preserving
related dialogs, popups, and other window-family identities. Tablets,
foldables, and external displays may offer a freeform or desktop-like canvas.
The destination owns safe-area handling, orientation, Android navigation, and
shell gestures while projecting its available logical extent and scale to the
source. Target modes and quality disclosure follow
[Remote presentation targets and quality](remote-presentation.md).

Touch translation, native touch, IME, selection, and shell-owned navigation
follow the Direction-level
[presentation-target input
contract](remote-presentation.md#presentation-target-model--direction)
rather than being defined by the mobile assembly.

### Godot/Rust phone-first XR client

As of 2026-09-11, the next intended client experiment uses Godot with
[godot-rust/gdext](https://github.com/godot-rust/gdext). Begin on an Android phone
even though a headset is available: build/deploy, logs, input and lifecycle are
easier to iterate on without repeatedly entering and leaving VR. This is an
Exploration, not an installed toolchain or an implemented Android client.

The proposed split keeps protocol/session/input mechanisms in reusable Weld Rust
code, exposes a small Godot-facing integration, and keeps Android codec and
presentation objects behind a platform adapter. Godot supplies shell scenes,
layout and eventual OpenXR integration; most behavior may be written in Rust.
The encoded/Iroh libraries now have compositor-free default dependency graphs;
Linux native integration is opt-in. See the implemented
[portable receiver boundary](../receiver-decoder-pool.md#portable-encoded-receiver-and-iroh-binding).
The Godot shell is not yet wired to those libraries. Do not ship
Smithay/DRM/VA-API host machinery just to reuse hoist policy.

The [godot-rust Android guide] and the user's supplied [Android build report]
provide a starting point: an ARM64 Rust `cdylib`, `cargo-ndk`, Godot's Android
export templates, and APK packaging/signing. Pin compatible Godot, gdext,
SDK/NDK and JDK versions when implementing, rather than blindly copying the
report's SDK 37 example. Package the library inside the Godot project, keep
signing secrets out of version control, and avoid release builds or broad
dependency rebuilds for ordinary iteration.

Suggested validation order:

1. A minimal phone APK that calls Rust through GDExtension, with repeatable debug
   build/deploy and lifecycle logging.
2. One hardware-decoded video panel, preferably AV1 when exposed by the device.
   Probe MediaCodec format/profile/extent support and real decode-to-presentation
   behavior; software fallback or H.264 must be an explicit result, not a hidden
   substitute for validating the selected hardware path.
3. One real Iroh hoist session with resize, scale, basic input, reclaim,
   disconnect and Android pause/resume. Keep networking and codec work off the
   shell's frame-critical path. Start on a convenient local network; repeat
   isolated mobile-network validation after the client itself is dependable.
4. Reuse the client/Android work in a Pico OpenXR shell, first as one ordinary
   mono window with controller interaction. Validate headset-specific surfaces,
   frame pacing and resume independently; phone success is not XR validation.

The Pico experiment must use Godot's standard OpenXR integration, not the Pico
XR Godot extension or a Pico-specific SDK. Building and running this client must
not depend on a Pico developer account or vendor login. Validate the required
OpenXR loader, Android packaging and runtime capabilities on the headset before
building on them. Optional OpenXR features must be capability-gated; if a path
requires the excluded vendor integration, report that limitation and use a
standard OpenXR alternative or defer the feature rather than add that dependency.
The phone-first validation order remains unchanged.

For the phone, investigate MediaCodec output through Godot's [ExternalTexture]
or another native GPU presentation adapter. For XR, also evaluate Godot's
[OpenXR composition layers], including their Android Surface path. That path
could avoid a CPU pixel download and potentially an extra shell composition
pass, but runtime support, synchronization, buffer lifetime and layer limits
must be demonstrated. No per-frame raw-pixel CPU readback in the intended path.

Pico advertises [AV1 decoding on the Pico 4 Ultra]; usable profiles, cadence and
concurrent decoders still require on-device validation. Do not assume identical
capabilities on the phone or other Pico models. The Ultra does not have built-in
eye tracking; [Pico's tracking compatibility] lists other eye-equipped models.
Head/controller-directed regions can exercise the foveated-media mechanics,
but cannot validate eye-gaze accuracy or gaze-to-quality latency.

This sequence takes precedence over the earlier Dioxus/Blitz-first experiment;
those remain alternatives, including for a future device-wallet UI. Stereo
application protocols, gaze-driven enhancement, full workspace takeover and a
complete spatial window manager are not prerequisites for the first panel.

[godot-rust Android guide]: https://godot-rust.github.io/book/toolchain/export-android.html
[Android build report]: https://github.com/godot-rust/gdext/issues/470#issuecomment-4587348846
[ExternalTexture]: https://docs.godotengine.org/en/4.6/classes/class_externaltexture.html
[OpenXR composition layers]: https://docs.godotengine.org/en/4.6/classes/class_openxrcompositionlayer.html
[AV1 decoding on the Pico 4 Ultra]: https://www.picoxr.com/global/products/pico4-ultra
[Pico's tracking compatibility]: https://developer.picoxr.com/blog/native-sdk-3/

### Existing-compositor hoisting proxy

The [Wayland proxy exploration](wayland-proxy.md) would expose Weld's hoisting
while Sway, Hyprland or another compositor retains desktop management. Its goal
is daily use of networking/convergence without first completing Weld's own WM.
It is separate future work, not a prerequisite for the phone/headset experiment.

Its host-window presenter should also support a receiver-only assembly: connect
to remote Weld instances and display their windows in the existing compositor,
without hosting local applications or offering source/onward hoisting. The
[presenter boundary](wayland-proxy.md#reusable-presenter-and-receiver-only-assembly--direction)
keeps that mode independent of the local application proxy.

### Linux-native XR desktop

A Linux headset can be a complete Weld desktop rather than only a thin remote
viewer. Local Wayland applications should run natively against the headset's
Weld instance and enter the same client, window, seat, and presentation models
as applications adapted from another Weld device. Native applications bypass
media encoding and transport entirely; only remote or otherwise isolated
applications pay that cost. The spatial shell must not make local content take
a streaming round trip merely to give local and remote windows a uniform UI.

The first tracer can run as an ordinary OpenXR destination on an existing Linux
desktop. If a headset platform permits replacement desktop software, a later
distribution may instead combine the compositor, spatial shell, and OpenXR
integration into the primary user environment. The boundary remains the same:
OpenXR supplies headset views, timing, tracking, and composition integration;
Weld owns application presentation, window relationships, logical seats,
authorization, and local-versus-hoisted source selection. Raw DRM ownership and
XR-runtime integration are platform decisions, not requirements of the client
or hoist protocols.

An open, Arch-based standalone headset such as Valve's announced Steam Frame is
a promising validation target, but the design must not depend on one product's
installation policy, compositor stack, codec API, or runtime privileges. Media
backends are capability-selected: a headset may expose VA-API, Vulkan Video, a
platform codec, software decode, or no codec path at all for local content.

This distribution should compose existing mechanisms rather than introduce a
parallel VR protocol:

- local Wayland and other native adapters provide client surfaces directly;
- hoist adapters provide remote client surfaces and ordered input routes;
- the spatial presentation target places both kinds of windows in one canvas;
- application view sets optionally provide fixed or head-tracked stereo;
- logical seats route keyboard, pointer, controller, hand, and accessibility
  input; and
- media negotiation and budgeting apply only where content crosses a transport
  or another isolation boundary.

For streamed XR content, eye tracking can drive a low-detail full-view base plus
high-detail gaze-local enhancement regions. Raw gaze stays on the headset by
default, while the media protocol receives only predicted normalized regions.
This foveated streaming path is independent of application foveated rendering
and is unnecessary for native local windows unless another isolation boundary
still requires encoding.

A workspace can be handed to the headset as a sliding set, spatial field, or
unbounded canvas while the source retains an emergency reclaim path. Shared
spatial workspaces and several independently focused users build on the same
multi-seat model; they do not change the local-first rule.

### Master desktop

The comprehensive desktop assembly could provide default window policy, bars,
launchers, composable [floating or tiling policy](window-management.md),
networking UI, and reloadable TOML or KDL configuration. It remains one
opinionated distribution rather than turning all of those choices into core
requirements.

The names are descriptive placeholders, not promised binary or package names.

## Packaging constraints — Direction

- Core compositor safety and protocol behavior must not depend on an optional
  distribution plugin being present.
- Native features and system dependencies should be selected deliberately by
  the distribution.
- Headless, nested, and DRM presentation should share application policy where
  their capabilities overlap.
- A plugin should compile against Weld's supported Bevy version and public
  facade rather than selecting an independent framework graph.
- Distribution configuration may select plugins and data; it must not punch
  through the Smithay/wgpu boundary.
