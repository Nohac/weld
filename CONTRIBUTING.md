# Contributing to Weld

## Start here

Read the [Weld specifications](docs/spec/README.md) for project intent and
future direction. Their status labels distinguish verified behavior from
agreed constraints and exploratory ideas; only Implemented material describes
the repository as it exists.

Before the first stable release, prioritize architectural coherence over
changeset size. Sweeping, cross-cutting refactors are acceptable when they
establish or correct ownership, module, API, and lifecycle boundaries. When
concrete responsibilities are already distinct, separate them before temporary
coupling becomes the project structure. Add crates only for real dependency,
runtime, reuse, or testing boundaries, and do not create empty modules or
placeholder crates for future milestones. The current crate responsibilities
and dependency direction are recorded in [Architecture](docs/architecture.md).

For Bevy-related work, read the relevant project skill first:

- [bevy-019](.agents/skills/bevy-019/SKILL.md) for Bevy 0.19 APIs and Weld's
  integration boundaries.
- [bevy-bsn-ui](.agents/skills/bevy-bsn-ui/SKILL.md) for BSN scenes and reusable
  UI composition.

For compositor-host work, read [smithay](.agents/skills/smithay/SKILL.md) before
changing protocol dispatch, event-loop integration, backends, rendering,
Smithay features, vendored source, or local upstream patches.

Tracked architecture evidence belongs under `docs/`; agent workflows and
reusable implementation guidance belong under `.agents/skills/`.
Read [Architecture](docs/architecture.md) before changing subsystem ownership
or lifecycle boundaries.

## Running tools

Use debug-profile commands during normal development. Run the narrowest useful
check first, then widen as the change warrants:

```text
cargo fmt --check
cargo check --workspace
cargo test <test-name>
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Debug builds apply light optimization to Weld and full optimization to
dependencies, following Bevy's recommended development profile. They also link
Bevy through the published `bevy_dylib` development helper so iterative links
stay short. The `profiling-tracy` feature is the exception: it links Bevy into
the executable so the native Tracy client is not split across a dynamic-library
boundary. Run debug executables through Cargo, which supplies the runtime search
path for `libbevy_dylib` and Rust's shared library. Invoking `target/debug/weldwm`
directly requires an equivalent `LD_LIBRARY_PATH`.

A standalone library build such as `cargo build -p weld-app` has no final
executable in which Cargo can use `bevy_dylib`. It therefore builds a separate
static-compatible dependency graph instead of reusing the distribution's
dynamic graph. This is a Cargo linkage boundary, not a Bevy feature mismatch.

Unit-test binaries remain statically linked. Force-linking `bevy_dylib` from a
test target makes Cargo rebuild the dependency graph in `prefer-dynamic` mode,
which is slower and substantially increases `target/` size.

All workspace crates that use Bevy inherit one exact version and feature set
from the root workspace dependency. Keep that set centralized: crate-local
feature additions make package-specific Cargo commands build another Bevy
artifact instead of reusing the distribution's prebuilt Bevy artifacts.

Release executables do not reference the development dylib and remain
standalone. Cargo still builds an unused `libbevy_dylib` artifact because
dependencies cannot vary by profile; do not make the helper optional, since
that would require a feature flag for ordinary `cargo run`.

Cargo uses Clang and LLD for Linux targets. The shared Rust shell already
provides both tools, so run builds from that environment rather than depending
on globally installed linkers.

Bevy system-font discovery requires Fontconfig development metadata while
building and `libfontconfig` at runtime. The shared Rust shell provides both;
other environments must make Fontconfig available to `pkg-config` and the
runtime loader.

Run Weld inside a development shell whose glibc is compatible with the running
NixOS graphics drivers. The shared Rust shell is located at
`/home/jonas/Dotfiles/nixos/envs/rust`; reload it after its lock file changes.

Launch Weld's nested development host without a client:

```text
cargo run -- --backend nested
```

Backend selection defaults to `auto`. Use an explicit backend when validating
host-specific behavior so the environment cannot change the target silently.

Pass a program and arguments to launch it against Weld's private Wayland
socket. The verified smoke test uses foot:

```text
cargo run -- --backend nested -- foot
```

To open another application in an already-running Weld instance, use the
client launcher from a second shell. It connects the application to Weld's
`weld-0` Wayland socket and does not start another compositor:

```text
scripts/run-app foot
```

The launcher forces common toolkits onto their native Wayland backends and
disables X11 fallback.

The standard distribution provides these backend-neutral shortcuts:

- `Super+Enter`: launch foot
- `Super+F`: launch Firefox
- `Super+B`: launch Blender
- `Super+LMB`: move the floating window under the pointer
- `Super+RMB`: resize the floating window under the pointer from its nearest corner
- `Super+Shift+O`: toggle output-topology diagnostics
- `Super+Shift+Escape`: exit Weld

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

### Tracy profiling

Use the opt-in Tracy integration and repeatable scenarios described in
[Profiling](docs/profiling.md). Profiling is a measurement workflow, not a
requirement for ordinary changes.

When auto selects the nested target, it runs until its host window is closed.

### DRM probes

Use the direct-wgpu probe as a historical diagnostic for Vulkan Display
discovery, presentation, and VT recovery:

```text
scripts/run-drm-wsi-probe --seconds 30
scripts/run-drm-wsi-probe --seconds 30 --switch-vt 1
```

Run it from a bare TTY and switch back before the deadline. A requested VT
cycle succeeds only after presenting a frame following activation. Also verify
that the destination VT's graphical compositor remains usable and that the
text console is restored after exit. Output defaults to
`/tmp/weld-drm-wsi-probe.log`; set `WELD_DRM_WSI_PROBE_LOG` to override it.

Use the Smithay output-compositor probe when changing the production DRM
boundary:

```text
scripts/run-smithay-drm-compositor-probe
```

Both probes require a real TTY. See
[Direct DRM presentation](docs/drm-presentation.md) and
[Smithay integration validation](docs/smithay-integration-validation.md) for
their scopes and acceptance evidence.

For dependency changes, edit only the intended dependency. If an existing
lockfile entry must move, use `cargo update -p <package> --precise <version>`;
never update the entire lockfile casually.

## Tests

Add coverage when it validates a stable behavior, protects a contract, or
reproduces a regression. Prefer deterministic ECS and policy tests over tests
that require a display server or GPU. Use a nested or headless host for broader
integration tests once one exists.

Test Weld-owned business logic and the contracts at integration boundaries.
Do not add tests that reassert Smithay behavior, or tests whose implementation
is mostly a thin call through Smithay APIs. Validate those dependencies through
focused integration probes on real hardware when needed, and keep unit tests
for policy, translation, lifecycle, and invariants that Weld itself owns.

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
- Do not suppress structural lints such as excessive function arguments by
  default. Prefer correcting the ownership boundary or introducing a cohesive
  context type; when an exception is genuinely clearer, document the concrete
  reason on the narrowest `#[expect(...)]`.
- Keep imports at module scope and use descriptive names rather than
  abbreviations.
- When every variant of an enum carries the same field, extract that field
  into an enclosing struct and keep only variant-specific data in the enum.
- Link Rust types as [`TypeName`] in doc comments when rustdoc can resolve them.

Document non-obvious ownership, ordering, lifetime, protocol, and thread
boundaries close to the implementation. Significant modules should explain
their purpose and normal consumer, but ordinary accessors and direct control
flow do not need narration.

## Commits

Make an atomic commit after each coherent batch. Use a short subject line and
omit the body unless it adds essential context. Never add generated-by or
co-author attribution trailers.
