# Overview

## Purpose — Direction

Weld is a programmable Wayland compositor and application framework built by
combining Smithay's protocol and hardware mechanisms, Bevy's application,
ECS, rendering, and UI facilities, and a project-owned wgpu presentation path.
It is meant to be assembled from reusable policy and presentation plugins
rather than fixed into one desktop experience.

Window-level remote hoisting is a defining future capability: applications
continue running under one Weld instance while a compatible destination
client—including another Weld instance, a native desktop client, a browser, or
a mobile app—presents their windows individually. One session may hoist a
single window family, a virtual workspace, or a complete desktop, but transport
preserves each window's identity instead of flattening the source into
screen-scraped video. It is application presentation relocation, not process
migration.

## Current foundation — Implemented

The workspace already separates the Smithay/wgpu host, the Bevy application
bridge, optional default window policy, and the standard distribution. It runs
nested under an existing desktop or directly on DRM and supports multiple
ordinary application windows and popups. See [Architecture](../architecture.md)
for the verified boundaries.

## Goals — Direction

Weld aims to:

- make window management, layout, focus, decorations, shell UI, and future
  remote behavior composable through Bevy plugins and systems;
- keep Smithay objects, Wayland resources, native handles, and raw wgpu
  internals behind typed host boundaries;
- compose client surfaces with ordinary Bevy primitives so transforms,
  clipping, shadows, text, and compositor UI share one scene;
- preserve the current efficient DMA-BUF and demand-driven paths while
  extending their format, output, and presentation capabilities;
- support nested, physical-display, and eventually headless or streaming
  presentation without changing application policy;
- make components replaceable while retaining safe defaults; and
- expose stable Weld identities at persistence, IPC, and network boundaries
  instead of Bevy entities or Smithay objects.

## Non-goals — Direction

Weld does not initially aim to:

- deliver a complete desktop environment, panel, launcher, lock screen, and
  settings suite as one indivisible product;
- promise a stable Rust ABI for arbitrary dynamically loaded plugins;
- give plugins unrestricted access to Smithay, DRM, Wayland, wgpu, or native
  graphics objects;
- build the complete remote product before the local compositor is reliable;
- flatten a workspace or desktop into screen-scraped video when window-level
  transport can preserve its structure; or
- implement every optional Wayland protocol or visual effect before the
  underlying lifecycle and presentation paths are sound.

## Runtime boundary — Implemented

Weld uses a real Bevy `App` and Bevy renderer. It does not give the outer
compositor loop to Bevy's window runner. In nested mode Weld pumps winit and
calloop from its own backend loop; the DRM backend does not use winit.
