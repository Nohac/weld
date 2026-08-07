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

The initial direction is to let the compositor host own Wayland and event-loop
mechanics while ECS code owns testable policy. Bevy's full application,
windowing, and rendering runtime is not a default dependency: add standalone
Bevy crates only when the current change needs them.

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

Use `cargo run` for the current binary. Add project-shell, nested-compositor, or
headless instructions here only after those paths exist and have been verified.

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
