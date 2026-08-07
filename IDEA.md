# Weld — Wayland Compositor Specification

**Status:** Initial specification
**Repository:** `weld`
**Primary crate/binary:** `weldwm`
**Language:** Rust
**Target platform:** Linux with Wayland
**License:** To be decided

## 1. Project Summary

Weld is a programmable Wayland compositor and window manager built around three architectural layers:

- **Smithay** for Wayland protocols, input, outputs, DRM, XWayland, and compositor plumbing.
- **`bevy_ecs`** for compositor state, scheduling, configuration, rules, layouts, and plugins.
- **`wgpu`** for GPU rendering, effects, decorations, shell surfaces, and reusable UI primitives.

Weld should feel like a system assembled from composable pieces rather than a monolithic desktop environment. Policies such as window placement, focus behavior, layouts, keybindings, decorations, animations, and remote-window behavior belong in ECS systems and plugins instead of being hard-coded into the protocol layer.

A defining future capability is **window hoisting**: moving the interactive presentation of an individual application window from one Weld system to another while preserving a placeholder in the original remote workspace.

Smithay is intentionally a low-level compositor framework and does not provide Weld’s window-management or drawing policy. It is built around the `calloop` event loop, making it suitable as Weld’s protocol and hardware host. `bevy_ecs` provides ordered schedules over a `World`, while `wgpu` provides the low-level native GPU abstraction needed for Weld’s renderer.

---

## 2. Goals

### 2.1 Primary goals

Weld must:

1. Provide a usable Wayland compositor supporting tiled and floating windows.
2. Make compositor policy programmable through Rust plugins and configuration.
3. Represent compositor state using a stable ECS-facing model.
4. Keep Smithay and raw Wayland objects out of plugins.
5. Provide a modern GPU-rendered visual system supporting:
   - Rounded corners
   - Borders
   - Shadows
   - Clipping
   - Text
   - Images and icons
   - Transparency
   - Transformations
   - Animations

6. Support efficient DMA-BUF-backed client rendering and damage tracking.
7. Support XWayland for legacy X11 applications.
8. Expose a stable, versioned IPC protocol.
9. Make remote window hoisting possible without redesigning the compositor.
10. Remain usable when optional effects, plugins, or remote features fail.

### 2.2 Secondary goals

Weld should:

- Support fast development and configuration iteration.
- Allow alternative layout and focus policies.
- Support headless and nested backends for testing.
- Make shell components replaceable.
- Support multi-seat and touch/tablet input eventually.
- Allow third-party tools to inspect compositor state without accessing internal objects.

---

## 3. Non-goals

The initial project will not attempt to:

- Build a complete desktop environment.
- Provide a panel, launcher, notification daemon, lock screen, and settings application in the first milestone.
- Use the full Bevy application runtime.
- Let plugins directly access Smithay, DRM, `wgpu`, or Wayland resource objects.
- Guarantee stable Rust ABI compatibility for arbitrary dynamic libraries.
- Implement an entire remote-desktop product before local compositor behavior is mature.
- Stream an entire remote desktop when a single-window transport is sufficient.
- Reproduce every wlroots-specific protocol immediately.
- Add extensive visual effects before direct scanout, frame pacing, and damage handling work correctly.

---

## 4. Architectural Principles

### 4.1 Smithay owns mechanisms

Smithay-facing code owns:

- Wayland globals and protocol dispatch
- Surface lifecycle
- Surface commits
- Input devices
- Seats
- Output discovery
- DRM/KMS
- DMA-BUF import
- Explicit synchronization
- Presentation timing
- XWayland
- Session and device handling
- Backend event-loop integration

This code translates external events into Weld’s stable internal event model.

### 4.2 ECS owns policy

The ECS world owns:

- Windows
- Workspaces
- Outputs
- Seats
- Focus state
- Layout state
- Rules
- Keybindings
- Decorations
- Animation state
- Shell state
- Remote connections
- Hoisted windows
- User-visible configuration

The ECS must not contain borrowed Smithay resources or objects with event-loop-dependent lifetimes.

### 4.3 Rendering consumes snapshots

The renderer receives an immutable or independently owned render snapshot produced after compositor policy has run.

Rendering must not mutate window-management policy.

### 4.4 Effects are requested, not performed

ECS systems produce typed effects such as:

```rust
enum CompositorEffect {
    ConfigureWindow {
        window: WindowId,
        size: PhysicalSize,
        states: WindowStates,
    },
    SetKeyboardFocus {
        seat: SeatId,
        target: Option<FocusTarget>,
    },
    RaiseWindow {
        window: WindowId,
    },
    CloseWindow {
        window: WindowId,
    },
    WarpPointer {
        seat: SeatId,
        position: LogicalPoint,
    },
    RequestFrame {
        output: OutputId,
    },
}
```

The Smithay host validates and applies these effects during the commit phase.

---

## 5. Workspace Structure

The repository should initially contain:

```text
weld/
├── Cargo.toml
├── crates/
│   ├── weldwm/
│   ├── weld-core/
│   ├── weld-protocol/
│   ├── weld-wl/
│   ├── weld-gfx/
│   ├── weld-ui/
│   ├── weld-config/
│   ├── weld-ipc/
│   ├── weld-remote/
│   └── weld-test/
├── config/
│   └── example/
├── protocols/
├── shaders/
├── docs/
└── examples/
```

### 5.1 `weldwm`

Executable and process-level orchestration.

Responsibilities:

- Parse command-line arguments
- Select backend
- Initialize logging
- Initialize Smithay
- Construct the ECS world
- Register built-in plugins
- Load user configuration
- Run the main event loop
- Coordinate shutdown and recovery

### 5.2 `weld-core`

Stable compositor model.

Contains:

- Stable IDs
- Components
- Resources
- Events
- Effects
- Schedule labels
- Plugin interfaces
- Layout interfaces
- Focus interfaces
- Configuration-facing APIs

This crate should have minimal knowledge of Wayland.

### 5.3 `weld-protocol`

Versioned serializable types shared by:

- IPC
- Remote transport
- Development tools
- State inspection
- Testing

Types in this crate must use stable Weld identifiers rather than ECS entities.

### 5.4 `weld-wl`

Smithay integration.

Contains:

- Wayland protocol handlers
- Surface-to-window mapping
- Input event translation
- Output/backend handling
- XWayland integration
- DMA-BUF handling
- Commit-effect application
- Frame callback management

### 5.5 `weld-gfx`

GPU renderer.

Contains:

- Surface import
- Scene composition
- Damage tracking
- Render passes
- Direct scanout integration
- Texture caching
- Shader management
- Output presentation
- Remote-frame import

### 5.6 `weld-ui`

Reusable visual and shell primitives.

Contains:

- Rounded rectangles
- Borders
- Shadows
- Text
- Images
- Clipping
- Layout nodes
- Hit testing
- Input regions
- Animation properties

### 5.7 `weld-config`

Configuration and plugin host.

Contains:

- Configuration API
- Built-in plugins
- Plugin registration
- Rules
- Keybindings
- Theme definitions
- User configuration loading
- Reload support

### 5.8 `weld-ipc`

Local IPC server and client library.

### 5.9 `weld-remote`

Optional remote-window transport and hoisting implementation.

### 5.10 `weld-test`

Test fixtures, fake backends, protocol harnesses, render comparisons, and network simulation.

---

## 6. Main Event Model

Smithay’s `calloop` loop remains the outer event loop. Weld does not replace it with Bevy’s application or window loop.

External events are accumulated and then processed in deterministic ECS stages.

```text
Smithay/calloop events
        │
        ▼
     Ingest
        │
        ▼
      Rules
        │
        ▼
      Layout
        │
        ▼
      Focus
        │
        ▼
     Effects
        │
        ▼
      Commit
        │
        ▼
 Render snapshot
        │
        ▼
 Render / Present
```

### 6.1 Schedule stages

#### `Ingest`

- Create or remove ECS entities
- Apply surface metadata updates
- Translate keyboard, pointer, touch, and tablet events
- Update outputs and seats
- Receive IPC commands
- Receive remote transport messages

No policy decision should be made by the Smithay callback itself unless required by protocol timing.

#### `Rules`

- Match newly mapped windows
- Assign initial workspace and output
- Decide tiled versus floating
- Apply opacity, border, and decoration rules
- Apply remote-window permissions
- Run user-defined rules

#### `Layout`

- Compute logical window geometry
- Respect minimum and maximum sizes
- Resolve workspace layouts
- Handle fullscreen and maximized windows
- Calculate layer-shell reserved areas
- Produce pending configure requests

#### `Focus`

- Resolve keyboard focus
- Resolve pointer focus
- Apply focus-follows-pointer or click-to-focus policy
- Maintain focus history
- Handle activation requests
- Resolve modal and popup focus constraints

#### `Effects`

- Convert changed policy state into explicit host operations
- Deduplicate configure requests
- Generate animation transitions
- Mark outputs as damaged
- Produce remote transport messages

#### `Commit`

- Apply effects through Smithay
- Send Wayland configures
- Update protocol-visible state
- Forward input
- Finalize remote state transitions

#### `Render`

- Build output-specific render snapshots
- Import client buffers
- Evaluate animations
- Compute visible damage
- Compose and present

The explicit schedule is a core compatibility boundary. Plugins may register systems into documented stages but may not invent ordering around host-critical operations.

---

## 7. ECS Data Model

ECS `Entity` values are process-local implementation details. All public APIs use stable typed identifiers.

```rust
struct WindowId(Uuid);
struct SurfaceId(Uuid);
struct WorkspaceId(Uuid);
struct OutputId(Uuid);
struct SeatId(Uuid);
struct RemotePeerId(Uuid);
struct HoistId(Uuid);
```

### 7.1 Principal entities

#### Window

Representative components:

```rust
struct WindowIdComponent(WindowId);
struct AppId(String);
struct WindowTitle(String);
struct WindowRole;
struct WindowGeometry(LogicalRect);
struct PendingGeometry(LogicalRect);
struct SizeConstraints;
struct WindowStates;
struct WorkspaceMembership(WorkspaceId);
struct OutputMembership(OutputId);
struct Tiled;
struct Floating;
struct Fullscreen;
struct Focusable(bool);
struct Activated(bool);
struct Mapped(bool);
struct DecorationStyle;
struct VisualState;
struct AnimationState;
struct SurfaceTreeRoot(SurfaceId);
```

A window is a window-management object, not necessarily a single `wl_surface`.

#### Surface

Represents protocol surfaces and surface trees.

Components may include:

- Stable surface ID
- Parent surface
- Surface role
- Buffer state
- Input region
- Opaque region
- Buffer transform
- Buffer scale
- Viewport state
- Damage
- Frame callback state

Smithay remains the source of truth for protocol object validity.

#### Workspace

Components may include:

- Workspace ID
- Name
- Index
- Output assignment
- Layout state
- Focus history
- Visibility
- Ordered window membership

#### Output

Components may include:

- Output ID
- Name
- Logical geometry
- Physical mode
- Scale
- Transform
- Refresh rate
- Enabled state
- Workspace set
- Damage state

#### Seat

Components may include:

- Seat ID
- Keyboard focus
- Pointer focus
- Grab state
- Cursor state
- Active output
- Input mode
- Shortcut inhibition state

#### Layer surface

Layer-shell surfaces should use distinct role components rather than pretending to be normal application windows.

#### Remote window

A remote window is represented using the same window-management components as a local window, with additional transport components:

```rust
struct RemotePeer(RemotePeerId);
struct RemoteSurface;
struct RemoteStreamId(u64);
struct DecodeState;
struct RemoteConfigureState;
struct RemoteInputState;
```

This allows layouts and focus policies to treat local and remote windows consistently.

---

## 8. Plugin System

### 8.1 Plugin contract

A plugin registers:

- Components
- Resources
- Events
- Systems
- Rules
- Commands
- Keybindings
- Layout implementations
- Visual primitives
- IPC handlers where permitted

Conceptually:

```rust
pub trait WeldPlugin {
    fn build(&self, app: &mut WeldApp);
}
```

`WeldApp` is an ECS/plugin builder, not the Bevy `App` type.

### 8.2 Stable facade

Plugins may depend on:

- `weld-core`
- Approved parts of `weld-ui`
- Approved serialized types from `weld-protocol`

Plugins must not depend directly on:

- Smithay resource types
- Raw Wayland objects
- DRM handles
- `wgpu::Device`
- Internal render graph objects
- `calloop` internals

### 8.3 Capability handles

Privileged actions are exposed through opaque capability resources:

```rust
struct WindowControl;
struct WorkspaceControl;
struct OutputControl;
struct ProcessSpawner;
struct RemoteControl;
```

A plugin requests actions through these APIs. It does not receive access to host internals.

### 8.4 Initial loading model

The first supported configuration model should be a normal Rust configuration crate compiled against Weld’s public facade.

The initial implementation may load this configuration statically. Development-mode dynamic reload can be added once the API boundary is sufficiently stable.

A failed reload must leave the last working configuration active.

### 8.5 ABI policy

Weld does not initially promise a stable native dynamic-library ABI.

Dynamic plugins must use one of:

- Exact Weld build compatibility
- A future stable C-compatible facade
- A future component/Wasm-based plugin boundary

The ECS component layout alone must not be treated as a stable binary ABI.

---

## 9. Configuration as Code

A Weld configuration should resemble application composition:

```rust
use weld_config::prelude::*;

pub fn configure(app: &mut WeldApp) {
    app.add_plugin(DefaultCompositorPlugin)
        .add_plugin(TilingPlugin)
        .add_plugin(RemoteHoistingPlugin);

    app.bind(modifier("SUPER") + key("Enter"), spawn("foot"));
    app.bind(modifier("SUPER") + key("Q"), close_focused());
    app.bind(modifier("SUPER") + key("H"), focus(Direction::Left));

    app.rule(
        window_rule()
            .app_id("org.pwmt.zathura")
            .workspace("reading"),
    );

    app.rule(
        window_rule()
            .title_contains("Picture-in-Picture")
            .floating()
            .always_on_top(),
    );
}
```

Configuration should support:

- Key and button bindings
- Startup commands
- Environment variables
- Window rules
- Output configuration
- Workspace definitions
- Layout selection
- Focus policies
- Theme selection
- Animation settings
- Remote-peer definitions
- Security permissions

Serialized files may be used for generated state or simple theme data, but the primary configuration model remains Rust code.

---

## 10. Layout System

Layouts are ECS policy modules.

A layout receives:

- Available output region
- Ordered set of tiled windows
- Window constraints
- Current layout state
- User layout commands

It returns desired geometry and visibility.

```rust
trait Layout {
    type State;

    fn arrange(
        &self,
        state: &mut Self::State,
        area: LogicalRect,
        windows: &[LayoutWindow],
        output: &mut Vec<LayoutAssignment>,
    );
}
```

Initial built-in layouts:

- Horizontal split
- Vertical split
- Master-stack
- Monocle
- Floating workspace

The architecture must allow future layouts such as:

- BSP
- Columns
- Tabbed containers
- Scrollable layouts
- Nested layout trees

Layout results are logical state. Animation systems interpolate visual geometry separately so layout implementations do not need to implement animations.

---

## 11. Focus and Input

Focus policy must be replaceable without changing Smithay handlers.

Supported initial policies:

- Click to focus
- Focus follows pointer
- Directional keyboard focus
- Focus history
- Explicit workspace focus restoration

Input processing must distinguish:

- Physical input events
- Compositor keybindings
- Client-forwarded events
- Remote-forwarded events
- Synthetic testing events

Input events should carry monotonic timestamps and seat identity.

Grabs, popups, lock surfaces, drag-and-drop, pointer constraints, and shortcut inhibition must be validated in the Smithay layer before forwarding.

Remote input must never be injected as an unrestricted local physical device. It must remain scoped to the authorized remote window or explicitly authorized remote seat.

---

## 12. Rendering Architecture

### 12.1 Renderer ownership

`weld-gfx` owns the `wgpu` objects and render graph.

The renderer consumes a scene containing:

- Imported client surfaces
- Solid geometry
- Text
- Images
- Shadows
- Decorations
- Cursors
- Shell overlays
- Remote video frames
- Debug overlays

### 12.2 Bevy-inspired UI primitives

Weld should reuse suitable standalone Bevy crates or implementation concepts where practical, but must not adopt Bevy’s window/event loop as the compositor host.

The primitive model should support:

```rust
struct UiNode {
    rect: LogicalRect,
    transform: Transform2D,
    opacity: f32,
    clip: Option<ClipShape>,
    background: Option<Background>,
    border: Option<Border>,
    border_radius: CornerRadii,
    shadows: Vec<BoxShadow>,
    content: UiContent,
    z_index: i32,
}
```

Required primitives:

- Rectangle
- Rounded rectangle
- Border
- Box shadow
- Text run
- Image
- Surface texture
- Video texture
- Clip rectangle
- Rounded clip
- Transform
- Opacity group

These primitives are used for both window decoration and compositor-owned UI.

### 12.3 Clipping

Clipping must support nested clips.

The implementation may use:

- Scissor rectangles for axis-aligned rectangular clips
- Stencil buffers for complex nested clips
- Analytic fragment clipping for rounded rectangles

The renderer should choose the cheapest valid mechanism.

### 12.4 Rounded corners and shadows

Rounded corners should not require modifying client buffers.

They should be applied when compositing the surface tree.

Shadows should be cached when practical and excluded from client-content damage calculations unless their geometry or style changes.

### 12.5 Text

Text rendering must support:

- Font fallback
- Subpixel-aware scaling
- High-DPI output
- Cached glyph atlases
- Bidirectional text where supported by the selected shaping stack

Text layout and glyph preparation must not block the presentation path unnecessarily.

### 12.6 Damage tracking

Weld must track damage at several levels:

- Client surface damage
- Surface-tree damage
- Window visual damage
- Animation damage
- UI-node damage
- Output damage

A full-output redraw should occur only when required.

### 12.7 Direct scanout and overlays

Fullscreen and compatible surfaces should be eligible for direct scanout.

Visual effects must not permanently prevent direct scanout. Weld may suppress decorations, shadows, and animations when entering a direct-scanout path.

Smithay’s DRM compositor can assign compatible elements to hardware planes and fall back to composition when necessary.

### 12.8 Frame pacing

Presentation should be driven by output timing rather than an unrestricted application update loop.

ECS policy systems run when:

- External state changes
- Input arrives
- Timers fire
- Animation requires another frame
- A client commits
- A remote frame arrives
- IPC changes compositor state

---

## 13. Wayland Protocol Support

### 13.1 Required for initial usability

- Core Wayland compositor
- Subcompositor
- Shared memory buffers
- XDG shell
- Seat
- Output
- Data device
- Viewporter
- Presentation time
- Linux DMA-BUF
- Relative pointer
- Pointer constraints
- Keyboard shortcuts inhibit
- Idle inhibit
- XDG activation
- XDG decoration
- Fractional scaling
- Layer shell
- Primary selection
- Text input as required for common clients

### 13.2 Subsequent support

- Output management
- Output power management
- Gamma control
- Screencopy/image-copy capture
- Foreign toplevel management
- Virtual pointer
- Virtual keyboard
- Session lock
- Tablet input
- Color management
- Explicit synchronization
- Content type and tearing control where appropriate

The image-capture protocol family separates capture sources from copying captured images into client-provided buffers. This model aligns well with Weld’s remote-window architecture, although protocol availability and interoperability must be evaluated during implementation.

### 13.3 XWayland

XWayland support must include:

- Startup and shutdown
- Mapping X11 windows to Weld windows
- Override-redirect windows
- Clipboard interoperability
- Focus integration
- Fullscreen handling
- X11 size hints
- XWayland keyboard grabs where required

XWayland policy must use the same ECS window model as native Wayland windows.

---

## 14. IPC

### 14.1 Transport

Local IPC should use a Unix-domain socket under the user runtime directory.

### 14.2 Protocol

Use a versioned JSON-RPC-style protocol initially.

Example:

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "window.focus",
  "params": {
    "window_id": "..."
  }
}
```

### 14.3 Required operations

- List windows
- Inspect a window
- Focus a window
- Close a window
- Move a window
- Change workspace
- List workspaces
- List outputs
- Execute compositor commands
- Reload configuration
- Subscribe to events
- Query remote peers
- Hoist or return a remote window

### 14.4 Events

Subscriptions should include:

- Window mapped
- Window unmapped
- Window metadata changed
- Focus changed
- Workspace changed
- Output changed
- Configuration reload result
- Remote peer connected
- Hoist state changed

IPC messages must never expose Smithay resource identifiers or ECS entity indices.

---

## 15. Remote Window Hoisting

### 15.1 Concept

Two Weld compositors can establish a trusted connection.

A window running on compositor **A** can be hoisted to compositor **B**:

1. The application continues running on A.
2. A retains the real Wayland client and surface.
3. The window’s visual content is captured and encoded on A.
4. B creates a locally managed remote-window entity.
5. B controls the window’s local placement and requested content size.
6. Input directed at the remote window is forwarded to A.
7. A leaves a placeholder in the original workspace.
8. The placeholder can restore local control, preview the window, or end the hoist.

This is window relocation at the compositor level, not application migration.

### 15.2 Placeholder

The remote placeholder must preserve:

- Original workspace membership
- Original tiled/floating state
- Original layout position
- Window identity
- Application title and icon
- Hoist destination
- Connection status

Initial controls:

- **Take Back** — terminate the remote presentation and restore the real window.
- **Peek** — temporarily display the current remote content.
- **End** — close the hoist session, with explicit behavior for whether the application remains running.

The placeholder should participate in layout as though the window were still present, preventing the remote workspace from unexpectedly collapsing or rearranging.

### 15.3 Local remote-window behavior

On B, the hoisted window should behave like a normal managed window for:

- Tiling
- Floating
- Workspace movement
- Focus
- Fullscreen
- Decorations
- Rounded corners
- Shadows
- Local animations

Protocol-specific behaviors remain implemented on A.

### 15.4 Configure flow

When B changes the visible size:

1. B sends a desired logical content size.
2. A translates this into an XDG configure.
3. The client commits a buffer for the new size.
4. A begins transmitting frames at the updated dimensions.
5. B updates the displayed content once the new frame is available.

Resize interaction must tolerate delayed client responses without blocking the local compositor.

B may temporarily scale the previous frame during interactive resize.

### 15.5 Transport split

Control and media must be logically separate.

#### Reliable control channel

Carries:

- Authentication
- Peer capabilities
- Window metadata
- Hoist requests
- Configure requests
- State transitions
- Clipboard metadata
- Input state requiring ordered delivery
- Error and shutdown messages

#### Low-latency media channel

Carries:

- Encoded video frames
- Frame timestamps
- Keyframe information
- Damage information
- Cursor metadata where required

#### Input path

Pointer motion may use unordered or replaceable messages. Button, key, focus, and modifier transitions require ordered handling.

### 15.6 Initial transport

The first proof of concept should use QUIC through a Rust implementation such as Quinn:

- TLS-secured peer connection
- Reliable streams for control
- Datagram support for latency-sensitive replaceable information
- Independent streams where useful

Quinn exposes bidirectional and unidirectional streams as well as unreliable unordered application datagrams, fitting the proposed control/media split.

The transport must remain behind a Weld interface so WebRTC can later be evaluated for:

- NAT traversal
- Congestion control
- Media interoperability
- Adaptive bitrate
- Existing hardware-codec integrations

### 15.7 Encoding

Preferred path:

```text
Client DMA-BUF
    → compositor capture
    → hardware encoder
    → network
    → hardware decoder
    → imported GPU texture
    → Weld scene
```

Fallback paths may use GPU copies or CPU-accessible frames when zero-copy operation is unavailable.

Initial codecs should be selected based on available hardware support rather than hard-coded as a compositor-wide requirement.

### 15.8 Audio and clipboard

These are separate capabilities.

Initial hoisting need not include audio.

Later versions may support:

- Per-application PipeWire audio routing
- Remote audio playback
- Clipboard synchronization
- Drag-and-drop transfer
- File-transfer approval prompts

PipeWire provides APIs for capturing video frames and is a likely integration point for broader media routing, although direct compositor-owned DMA-BUF capture should be preferred where practical.

### 15.9 Hoist state machine

```text
Local
  │ request
  ▼
Offering
  │ accepted
  ▼
Preparing
  │ first decodable frame
  ▼
Hoisted
  │ disconnect
  ▼
Recovering
  ├── reconnect → Hoisted
  └── timeout   → Local
```

Additional terminal state:

```text
Any active state → Ending → Local or Closed
```

The source compositor must remain capable of restoring the window after network failure.

### 15.10 Security

Remote functionality is privileged.

Requirements:

- Explicit peer pairing
- Persistent peer identity
- Encrypted transport
- Per-peer permissions
- Explicit authorization to hoist a window
- Clear indication that input is being forwarded
- Input scoped to the authorized window
- No unrestricted synthetic-input access by default
- Clipboard and audio permissions negotiated separately
- Revocable sessions
- Audit logging for connection and hoist transitions

A remote peer must never receive arbitrary access to the local Wayland socket or Smithay objects.

---

## 16. Error Handling and Recovery

Weld must recover gracefully from:

- Client crashes
- XWayland crashes
- GPU device loss where recoverable
- Output hotplug
- Configuration compilation failure
- Plugin initialization failure
- Remote decoder failure
- Remote network interruption
- Invalid IPC requests

A plugin error must not corrupt the protocol host.

Where Rust panics cannot be isolated safely, the process should fail clearly rather than continue with potentially invalid compositor state. Critical state required for remote restoration should be persisted or recoverable from the source compositor.

---

## 17. Logging and Diagnostics

Use structured tracing.

Every relevant operation should carry stable identifiers:

- Window ID
- Surface ID
- Output ID
- Seat ID
- Peer ID
- Hoist ID

Diagnostic facilities should include:

- ECS state inspection
- Schedule timing
- Render-pass timing
- Output damage visualization
- Surface-tree visualization
- Direct-scanout reason reporting
- Frame-latency metrics
- Remote bitrate, packet loss, decode latency, and queue depth
- Protocol event tracing behind explicit debug flags

---

## 18. Testing Strategy

### 18.1 Unit tests

Cover:

- Window rules
- Layout algorithms
- Focus policies
- Workspace transitions
- Effect deduplication
- Stable ID mappings
- Remote state machines
- Protocol serialization

### 18.2 ECS schedule tests

Run the core schedule with a synthetic world and verify:

- Deterministic stage ordering
- No host operations before `Commit`
- Correct configure generation
- Correct focus resolution
- Plugin ordering constraints

### 18.3 Wayland integration tests

Use nested or headless backends to test:

- Mapping and unmapping
- XDG configure sequences
- Popup behavior
- Layer-shell reservation
- Input forwarding
- Clipboard behavior
- XWayland mapping

### 18.4 Rendering tests

Include:

- Golden images for primitives
- Rounded clipping
- Nested clipping
- Borders and shadows
- Fractional scaling
- Multi-output transforms
- Damage-region correctness
- Remote texture rendering

Golden tests must allow controlled tolerance for GPU-dependent rasterization.

### 18.5 Remote tests

Simulate:

- Latency
- Jitter
- Packet loss
- Reordering
- Disconnects
- Resize storms
- Decoder stalls
- Source-window destruction
- Peer revocation

---

## 19. Performance Requirements

Weld should be designed around the following requirements:

- No continuous redraw while the desktop is idle.
- No full-output redraw for isolated surface damage unless required by an effect.
- Input processing must not wait for rendering or remote encoding.
- Remote encoding must not block the compositor thread.
- Shader compilation must not occur on the presentation-critical path.
- Window rule and layout evaluation should scale with affected windows where practical.
- Interactive resize must remain responsive when the client or remote peer is slow.
- Fullscreen clients should remain eligible for direct scanout.
- Remote frame queues must favor recent frames over displaying stale frames.

Performance measurements should be added before introducing strict numeric budgets.

---

## 20. Development Workflow

Recommended workspace practices:

- Keep protocol, ECS, renderer, and remote crates independently testable.
- Pin compatible dependency versions in the workspace.
- Use `sccache`.
- Use `mold` or `lld` for development linking.
- Use optimized dependencies in development profiles where beneficial.
- Cache shaders or compile them during the build.
- Feature-gate hardware backends and remote functionality.
- Provide a nested development mode that runs Weld inside an existing desktop session.
- Provide a deterministic headless mode for CI.

---

## 21. Implementation Roadmap

### Milestone 0 — Compositor skeleton

Deliver:

- Cargo workspace
- `weldwm`, `weld-core`, and `weld-wl`
- Nested backend
- Basic DRM backend
- XDG shell
- Keyboard and pointer input
- One output
- One workspace
- Basic floating placement
- ECS ingest and commit cycle
- Simple surface rendering
- Clean startup and shutdown

Exit criterion: common Wayland clients can open, receive input, render, resize, and close.

### Milestone 1 — Window-management foundation

Deliver:

- Stable IDs
- Multiple workspaces
- Tiled and floating state
- Built-in layouts
- Focus policies
- Window rules
- Keybindings
- Rust configuration crate
- Versioned local IPC
- Layer shell
- Basic XWayland

Exit criterion: Weld is usable as a basic daily window manager in nested testing.

### Milestone 2 — Visual system

Deliver:

- `weld-gfx`
- `weld-ui`
- Rounded rectangles
- Borders
- Shadows
- Text
- Nested clipping
- Decorations
- Animation framework
- Damage tracking
- Fractional scaling
- DMA-BUF import
- Direct-scanout eligibility
- Render diagnostics

Exit criterion: effects and decorations work without fundamentally compromising frame pacing or damage behavior.

### Milestone 3 — Plugin boundary

Deliver:

- Public plugin API
- Capability handles
- Stable schedule labels
- Configuration reload
- Plugin error reporting
- Example layout plugin
- Example focus plugin
- API compatibility policy

Exit criterion: layout and policy changes can be implemented outside the Smithay integration crate.

### Milestone 4 — Single-window hoisting prototype

Deliver:

- Peer pairing
- QUIC control transport
- Single remote window
- Software encode/decode fallback
- Forwarded pointer and keyboard input
- Source-side placeholder
- Take Back action
- Resize/configure round trip
- Disconnect recovery

Exit criterion: a window running on one Weld instance can be interactively managed on another instance over a trusted local network.

### Milestone 5 — Production remote path

Deliver:

- Hardware encode/decode
- DMA-BUF-oriented capture path
- Adaptive bitrate
- Damage-aware frame decisions
- Multiple simultaneous remote windows
- Strong permission model
- Clipboard capability
- Improved reconnection
- Transport evaluation for WebRTC
- Remote diagnostics

Exit criterion: remote hoisting is secure, resilient, and practical for sustained use.

### Milestone 6 — Desktop completeness

Potential work:

- Session locking
- Output management
- Tablet support
- Color management
- Accessibility integration
- Shell component ecosystem
- Notification and launcher integrations
- More robust plugin isolation

---

## 22. Key Architectural Decisions

The following decisions should be treated as foundational:

1. The project is named **Weld**.
2. The repository is `weld`; the primary crate and binary are `weldwm`.
3. Smithay is the compositor mechanism layer.
4. `bevy_ecs` is used without adopting Bevy’s application event loop.
5. `wgpu` is the rendering foundation.
6. Bevy-inspired UI and drawing primitives are reused or adapted where they can remain compositor-friendly.
7. Plugins operate on a stable Weld model, not Smithay objects.
8. External events enter ECS through an ingest boundary.
9. ECS produces typed effects that the host validates and commits.
10. Local and remote windows share the same window-management abstraction.
11. The source compositor remains authoritative for a hoisted application.
12. A hoisted window leaves a functional placeholder at its source.
13. Remote transport is optional and isolated from the core compositor.
14. Correctness, frame pacing, DMA-BUF handling, and damage tracking take priority over visual effects.
15. Configuration is primarily Rust code.

---

## 23. Initial Definition of Done

The first meaningful Weld release is complete when:

- Weld starts on real DRM hardware and in nested mode.
- Native Wayland and XWayland applications can be launched.
- Windows can be tiled, floated, focused, resized, moved, and closed.
- Multiple workspaces and outputs function correctly.
- Configuration and rules are expressed through the public Rust API.
- Decorations use Weld’s GPU UI primitives.
- Damage tracking prevents unnecessary redraws.
- Fullscreen applications can use an efficient presentation path.
- IPC can inspect and control compositor state.
- The Smithay layer, ECS policy layer, and renderer have clearly enforced boundaries.
- Automated tests cover layout, focus, protocol lifecycle, and rendering primitives.

Remote window hoisting remains part of the architecture from the beginning but is not required for the first local-compositor release.
