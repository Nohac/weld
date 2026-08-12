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
