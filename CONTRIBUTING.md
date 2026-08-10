# Contributing to Weld

## Start here

Weld is at the setup and design-validation stage. Read [IDEA.md](IDEA.md) for
the motivating direction, but treat it as a proposal rather than an
implementation checklist. Its crate layout and subsystem boundaries may change
as the first vertical slices teach us more.

Before the first stable release, prioritize architectural coherence over
changeset size. Sweeping, cross-cutting refactors are acceptable when they
establish or correct ownership, module, API, and lifecycle boundaries. When
concrete responsibilities are already distinct, separate them before temporary
coupling becomes the project structure. Keep one package until a real
dependency, runtime, reuse, or testing boundary justifies another crate; a
single package does not imply a single module. Do not create empty modules or
placeholder crates for future milestones.

For Bevy-related work, read the relevant project skill first:

- [bevy-019](.agents/skills/bevy-019/SKILL.md) for Bevy 0.19 APIs and Weld's
  integration boundaries.
- [bevy-bsn-ui](.agents/skills/bevy-bsn-ui/SKILL.md) for BSN scenes and reusable
  UI composition.

For compositor-host work, read [smithay](.agents/skills/smithay/SKILL.md) before
changing protocol dispatch, event-loop integration, backends, rendering,
Smithay features, vendored source, or local upstream patches.

## Current technical direction

The first validation slice is a single package using the restricted `bevy`
umbrella crate. Weld owns the outer winit window, Smithay server, event-loop
orchestration, and final wgpu presentation. Bevy supplies its app schedule,
renderer, UI primitives, and BSN scene composition, rendering both client
surfaces and shell UI into a Weld-owned texture through Bevy's manual
render-device path. Do not enable Bevy's window runner or expand its features
without a concrete need.

Smithay's renderer is deliberately outside this first slice. Weld accepts
multiple xdg-toplevels backed by `wl_shm`, copies their pixels into owned Bevy
images, and exposes their lifecycle through protocol-neutral ECS entities.
Readable subsurfaces above the toplevel root are ordered and positioned as
internal Bevy image layers behind the same project-owned `SurfaceNode`; the
root image stays on that node so its rounded clipping and root-only fast path
remain intact. The default window plugin independently claims and decorates
each mapped surface, so client content composes with ordinary Bevy UI. Smithay
remains responsible for Wayland protocol state and applies focus or close
actions chosen by ECS policy; it does not own window placement, stacking, or
decoration. The final project-owned wgpu pass only presents or captures Bevy's
completed texture. Weld advertises `xdg-decoration` and answers decoration
objects with server-side mode. Clients that do not bind the global retain
client-side decorations; the default presenter still adds shell chrome to those
clients until the next slice treats an absent decoration object as CSD and
routes `xdg_toplevel` move and resize requests. Committed
`xdg_surface.set_window_geometry` crops root-buffer shadow margins, defines the
plugin-facing `MappedSurface.logical_size`, and preserves root-surface input
coordinates. Geometry spanning subsurfaces outside the root buffer is not yet
represented. `ImageNode` is a provisional SHM backing, not the plugin-facing
surface contract. Below-root subsurface ordering, role-only subsurface
detachment without a later tree commit, precise subsurface input, popups,
dmabuf, damage-aware uploads, presentation timing, VRR, and HDR remain explicit
spike boundaries rather than settled compositor architecture.

Ordinary nested rendering is event driven. Host and client-surface changes
request a composition directly; Bevy systems that drive continuous visual
changes should emit `bevy::window::RequestRedraw` while they remain active.
Bevy primitives participate normally in a requested composition, but their
mutation is not a universal automatic invalidation signal. Frame pacing for a
continuous request stream is deferred; an active calloop source can currently
wake the host sooner than the nominal frame interval.

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

## Running tools

Use debug-profile commands during normal development. Run the narrowest useful
check first, then widen as the change warrants:

```text
cargo fmt --check
cargo check
cargo test <test-name>
cargo clippy --all-targets --all-features -- -D warnings
```

Debug builds apply light optimization to Weld and full optimization to
dependencies, following Bevy's recommended development profile. They also link
Bevy through the published `bevy_dylib` development helper so iterative links
stay short. Run debug executables through Cargo, which supplies the runtime
search path for `libbevy_dylib` and Rust's shared library. Invoking
`target/debug/weldwm` directly requires an equivalent `LD_LIBRARY_PATH`.

Release executables do not reference the development dylib and remain
standalone. Cargo still builds an unused `libbevy_dylib` artifact because
dependencies cannot vary by profile; do not make the helper optional, since
that would require a feature flag for ordinary `cargo run`.

Cargo uses Clang and LLD for Linux targets. The shared Rust shell already
provides both tools, so run builds from that environment rather than depending
on globally installed linkers.

Run the nested compositor inside a development shell whose glibc is compatible
with the running NixOS graphics drivers. The shared Rust shell is located at
`/home/jonas/Dotfiles/nixos/envs/rust`; reload it after its lock file changes.

Launch Weld without a client:

```text
cargo run
```

Pass a program and arguments to launch it against Weld's private Wayland
socket. The verified smoke test uses foot:

```text
cargo run -- foot
```

To open another application in an already-running Weld instance, use the
client launcher from a second shell. It connects the application to Weld's
`weld-0` Wayland socket and does not start another compositor:

```text
scripts/run-app foot
```

The launcher forces common toolkits onto their native Wayland backends and
disables X11 fallback.

Weld options precede an explicit `--` when a client is also present. Capture a
settled client-plus-shell composition and exit with:

```text
cargo run -- --screenshot target/weld-startup.png -- foot
```

Enable the restricted, loopback-only development protocol with:

```text
cargo run -- --remote-debug -- foot
uv run --project tools/remote-debug weld-debug status
uv run --project tools/remote-debug weld-debug screenshot target/weld-remote.png
```

Read [REMOTE_DEBUGGING.md](REMOTE_DEBUGGING.md) before changing the protocol,
capture completion, or exposed Bevy methods.

The nested target intentionally runs until its host window is closed. Client
warnings about unsupported optional Wayland protocols are expected for this
minimal slice.

For dependency changes, edit only the intended dependency. If an existing
lockfile entry must move, use `cargo update -p <package> --precise <version>`;
never update the entire lockfile casually.

## Tests

Add coverage when it validates a stable behavior, protects a contract, or
reproduces a regression. Prefer deterministic ECS and policy tests over tests
that require a display server or GPU. Use a nested or headless host for broader
integration tests once one exists.

Keep exploratory coverage proportional. Avoid broad suites around provisional
wiring before its behavior has settled.

## Rust style

- Handle fallibility without `panic!`, `unreachable!`, or `.unwrap()` in
  production paths.
- Prefer `if let` and let chains over deeply nested matching when they improve
  clarity.
- Avoid unsafe code. When it is unavoidable, document each unsafe block with a
  `SAFETY` comment that states the upheld invariant.
- Prefer `#[expect(...)]` with a reason over `#[allow(...)]` for necessary lint
  exceptions.
- Keep imports at module scope and use descriptive names rather than
  abbreviations.
- Link Rust types as [`TypeName`] in doc comments when rustdoc can resolve them.

Document non-obvious ownership, ordering, lifetime, protocol, and thread
boundaries close to the implementation. Significant modules should explain
their purpose and normal consumer, but ordinary accessors and direct control
flow do not need narration.

## Commits

Make an atomic commit after each coherent batch. Use a short subject line and
omit the body unless it adds essential context. Never add generated-by or
co-author attribution trailers.
