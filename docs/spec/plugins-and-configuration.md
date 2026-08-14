# Plugins and configuration

## Current extension boundary — Implemented

`WeldApp` wraps Bevy's real `App`, and `WeldAppExt` lets ordinary Bevy plugins
inspect the active backend and add systems or resources. `weld-app` re-exports
the exact supported Bevy version, while `weld-window` demonstrates a replaceable
policy plugin built without direct Smithay or raw wgpu access. See
[Architecture](../architecture.md).

The restricted remote-debug protocol currently provides development status,
method calls, and screenshots. Its contract is documented in
[Remote debugging](../../REMOTE_DEBUGGING.md). It is a debugging facility, not
the stable desktop-control IPC described below.

## Plugin contract — Direction

Plugins may own policy components, resources, systems, layouts, visual
presentation, commands, and bindings. Host-critical actions cross typed Weld
requests and are validated by core. Public boundaries must not expose Smithay
resources, native handles, raw wgpu objects, calloop internals, or Bevy entity
indices.

Weld does not initially promise a stable native dynamic-library ABI. External
plugins compile against a compatible Weld and Bevy API. A C-compatible or
component/Wasm boundary remains possible only if a real isolation or ecosystem
need justifies it.

## Reloadable configuration — Direction

Window, input, appearance, layout, binding, and remote policy should be stored
as application resources. Replacing those resources lets systems observe new
values while retaining clients and window state. Native changes cross typed
host requests; immutable roots may require controlled reinitialization.

The standard distribution should support a declarative configuration format
such as TOML or KDL for common composition, rules, themes, and bindings. Rust
plugins remain available when configuration needs new behavior rather than
data. The exact schema and reload transaction are not yet selected.

The Weld-owned settings model should cover the meaningful input and compositor
controls users rely on when adopting it without copying another compositor's
configuration shape. Resolution must preserve global defaults, device-type or
class overrides, and stable device-specific overrides with explicit
precedence. Future Sway, Hyprland, or other compatibility importers should
translate into those typed settings and report unsupported or lossy mappings
instead of silently changing their meaning. Hot reload and device add or resume
should apply the resolved settings only to affected devices while retaining
clients and window state.

A failed reload must leave the last working configuration active and report a
structured error.

Persistent-frame matching and saved layout state follow the project-owned
identity and rule boundaries in
[Window management](window-management.md#persistent-frames--direction).

## Administrative IPC — Direction

A versioned, user-scoped local IPC should support state inspection, window and
workspace actions, command execution, configuration reload, subscriptions,
screenshots, and future remote-session control. Messages use stable Weld IDs
and project-owned serialized types. Transport and encoding should be selected
when the first non-debug consumer exists rather than inherited accidentally
from the development protocol.

Remote app discovery and launch are part of the authenticated
[launcher federation](remote-hoisting.md#launcher-federation--direction), not
an implicit extension of local administrative IPC.

## Open work — Exploration

- Capability resources for privileged plugin actions.
- Configuration transaction and migration semantics.
- Plugin compatibility policy and safe failure isolation.
- A stable schema shared by local IPC, tooling, persistence, and selected
  remote control messages without coupling their transports.
