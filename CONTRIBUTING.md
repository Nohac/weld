# Contributing to Weld

## Start here

Weld is at the setup and design-validation stage. Read [IDEA.md](IDEA.md) for
the motivating direction, but treat it as a proposal to test through small
increments. It does not authorize implementing the complete roadmap, and its
crate layout and subsystem boundaries may change as the first vertical slices
teach us more.

Prefer one package and one direct implementation until a real dependency,
runtime, reuse, or testing boundary justifies another crate. Do not create
empty modules or placeholder crates for future milestones.

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
orchestration, and final wgpu composition. Bevy supplies its app schedule,
renderer, UI primitives, and BSN scene composition, rendering the shell into a
Weld-owned texture through Bevy's manual render-device path. Do not enable
Bevy's window runner or expand its features without a concrete need.

Smithay's renderer is deliberately outside this first slice. Weld currently
accepts one xdg-toplevel backed by `wl_shm`, copies its pixels into an owned wgpu
texture, draws that client layer, and then draws Bevy's transparent shell layer.
The fixed output size, full redraws, missing input routing, and lack of damage,
subsurface, popup, dmabuf, presentation-timing, VRR, and HDR support are explicit
spike boundaries rather than settled compositor architecture.

BSN and Bevy's UI work are references for composition, behavior, accessibility,
and state synchronization. Do not add Feathers by default. If Weld adopts Bevy
scene or headless-widget infrastructure, keep domain state authoritative
outside widgets and translate widget events into project-owned actions. We will
design a Weld-specific visual layer separately when a concrete UI slice exists.

Keep provisional decisions easy to reverse. Before making an architectural
change, describe the smallest problem it solves and the boundary it introduces.

## Running tools

Use debug-profile commands during normal development. Run the narrowest useful
check first, then widen as the change warrants:

```text
cargo fmt --check
cargo check
cargo test <test-name>
cargo clippy --all-targets --all-features -- -D warnings
```

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
