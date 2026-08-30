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
first native-shell experiment should use
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
is the preferred Blitz experiment. A native Android EGL surface embedded in
the shell remains a valid hardware path, and the Dioxus web renderer is an
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
