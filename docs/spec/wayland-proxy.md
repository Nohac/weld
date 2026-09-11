# Wayland proxy and hoisting host

## Motivation and status — Exploration

Let users keep an established compositor such as Sway or Hyprland while using
Weld's hoisting, networking, and eventual spatial destinations. This would make
daily testing practical without first matching the window-management maturity
of those desktops. It is a candidate distribution/presentation backend, not an
implemented mode or a commitment to support every compositor and extension.

The Android phone-first client experiment currently comes before this work;
see [Distributions](distributions.md#godotrust-phone-first-xr-client).

## Proposed placement — Exploration

```text
Wayland applications
        |
Weld proxy / hoist host
        +-- individual proxy windows --> existing desktop compositor
        +-- remote presentations ------> Weld client / headset
```

Applications connect to Weld as their Wayland server. Weld connects as a client
to the existing compositor and presents each application toplevel independently.
The host compositor retains physical outputs, tiling, workspaces, decorations
where applicable, and desktop shortcut policy. Weld retains the application's
surface relationships and routes presentation and input according to hoist state.

This is not nesting Sway inside a single Weld output window. Ordinary output
nesting exposes the nested desktop's composed outputs, not its individual client
connections. Nor is this scraping another compositor's windows after the fact.

`weld exec sway` is a possible future session-launcher experience, not a proposed
command that already works. It would require deliberate socket/environment and
activation wiring so applications connect to the proxy and the proxy connects
to Sway. Launching a compositor under a wrapper alone does not accomplish that.
Applications already connected directly to the host are outside the proxy;
transparent adoption of their live Wayland connections is not assumed.

## Reusable presenter and receiver-only assembly — Direction

The component presenting individual windows to the existing host compositor
must not require a local Wayland application server or source-hoisting role.
It consumes Weld's client/presentation contracts and returns input and window
requests against stable identities. Local proxied applications and remotely
received applications are alternative sources for that presenter, not separate
window presentation implementations.

A client-only assembly can therefore connect to a remote Weld instance and
present its streamed windows as normal windows in Sway or another supported
host. It is a receiving destination, but does not export local applications,
advertise a source catalogue or offer its received windows for onward hoisting.
There is no implicit re-export or proxy chain. Receiving, sending input back,
authorization and lifecycle control still require bidirectional communication;
client-only does not mean a one-way video player or a different transport.

Keep the local application-server adapter, shared host-window presenter and
source-hoist orchestration as concrete separate responsibilities when this is
implemented. A receiver must not start a local app socket, DRM/session backend
or source encoder merely to display remote windows. Receiving decode/import
resources remain necessary for the selected surface mode. Capabilities express
the enabled roles rather than treating every connected peer as a source.

The same presentation boundary can have other platform implementations, such as
the phone/Godot shell; this does not require sharing Wayland-native rendering
objects with Android. Package names and exact APIs remain undecided until a
concrete implementation plan establishes those dependencies.

## Local, hoisted, and reclaimed presentation — Exploration

Keep the same host-facing proxy toplevel alive through the transition:

- Locally, it presents application content and translates host input and
  configuration into the application's presentation contract.
- While hoisted, it presents a placeholder with information/reclaim controls.
  Application content and authorized input use the remote destination instead.
- On reclaim, request a client size appropriate for the retained local slot,
  then restore content once the appropriate replacement is ready.

Keeping the host-facing window is intended to preserve its tiling slot and
avoid disrupting other desktop applications. Exact placement remains the host
compositor's policy; proxying cannot promise to override that policy. Placeholder
geometry and remote application geometry must be independent: resizing or moving
the placeholder must not accidentally configure the remotely presented client.
The source-side application remains running, not migrated or restarted.

Reuse existing hoist family, tombstone, reclaim, authorization and recovery
semantics rather than create a proxy-specific hoist protocol. UI reuse does not
mean the current Bevy placeholder scene can already draw into this new backend.
The host's local window identity, the original client identity and the remote
presentation identity need explicit mappings, including title/app identity used
by host rules and how a user requests hoisting of the focused proxy window.

## Interoperability and cost — Exploration

This would be a stateful protocol boundary, not arbitrary byte forwarding.
Configuration acknowledgements, input serials and grabs, popup/subsurface
lifetimes, scaling/output feedback, frame callbacks, presentation feedback,
buffer release and explicit synchronization must have clear authority in each
state. Clipboard, drag-and-drop, pointer capture, IME and XWayland need explicit
coverage decisions. Advertise only capabilities the complete route can uphold.
Define host loss, remote loss and closing a placeholder without confusing them
with closing the application; preserve existing reclaim safety semantics.

For compatible local buffers, investigate forwarding DMA-BUF/SHM presentation
without a video encode or a full desktop render. Do not claim zero-copy before
validating format/modifier compatibility, synchronization and release lifetimes.
Use Smithay and existing Wayland client facilities where applicable; do not
reimplement host window management. Keep this native presentation adapter out of
transport-neutral hoist policy and independent of the choice of shell renderer.

Relevant prior art includes [Sommelier], which delegates composition to a host,
and [sommelier-rs], whose documented state-tracking proxy includes same-machine
operation. These demonstrate proxying, not Weld's ownership-switch/reclaim
behavior or a ready-made implementation for it.

## Smallest validation path — Exploration

Start with one explicitly launched application under an already-running Sway:
normal independent local window, remote hoist with the same local placeholder,
then reclaim without losing its layout slot. Exercise resize, scale, input,
popups, close and disconnect before expanding to everyday applications and other
compositors. Session-wide launching and activation interception come later.

This document records the idea only. It does not authorize placeholder crates,
new public interfaces, toolchain setup or implementation without a concrete plan.

[Sommelier]: https://chromium.googlesource.com/chromiumos/platform2/+/main/vm_tools/sommelier/README.md
[sommelier-rs]: https://github.com/google/sommelier-rs
