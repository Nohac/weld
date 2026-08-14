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

Launch Weld without a client:

```text
cargo run
```

Backend selection defaults to `auto`. A usable Wayland or X11 host selects the
nested backend, while a bare Linux virtual terminal selects standalone DRM.
Ambiguous environments fall back to the safer nested startup path. Use
`--backend nested` or `--backend drm` to override detection while debugging.

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

### Tracy profiling

Use the opt-in Tracy integration and repeatable scenarios described in
[Profiling](docs/profiling.md). Profiling is a measurement workflow, not a
requirement for ordinary changes.

When auto selects the nested target, it runs until its host window is closed.

### Standalone DRM backend

Run the standalone backend from an active TTY with a working logind or
seatd/libseat provider:

```text
cargo run
cargo run -- foot
```

On a bare virtual terminal these commands select DRM automatically. The
equivalent explicit override is `cargo run -- --backend drm -- foot`.
Set the standalone output scale with `cargo run -- --scale 2 -- foot`.
Fractional values are supported; nested mode follows its host compositor and
warns when an explicit scale is ignored.

Standalone input requires system libinput 1.26 or newer so Weld can set an
explicit one/two/three-finger left/right/middle clickfinger map. The shared
Rust development shell provides a compatible version.

Use the compositor shortcuts to launch clients or stop Weld:

- `Super+Enter`: foot
- `Super+F`: Firefox
- `Super+B`: Blender
- `Super+=` / `Super+-` (DRM only): increase or decrease output scale by 0.25
- `Super+Shift+Escape`: exit Weld
- `Ctrl+Alt+F1` through `Ctrl+Alt+F10` (DRM only): switch virtual terminal

`SIGINT` and `SIGTERM` also request an orderly shutdown, including when sent
from another VT. A real TTY run is required to validate a particular seat,
GPU, and display stack.

Use the standalone direct-wgpu probe to validate Vulkan display discovery,
presentation, and VT recovery independently from Weld's compositor backend:

```text
scripts/run-drm-wsi-probe --seconds 30
scripts/run-drm-wsi-probe --seconds 30 --switch-vt 1
```

Run it from a bare TTY and switch back before the deadline. A requested VT
cycle succeeds only after presenting a frame following activation. Also verify
that the destination VT's graphical compositor remains usable and that the
text console is restored after exit. Output defaults to
`/tmp/weld-drm-wsi-probe.log`; set `WELD_DRM_WSI_PROBE_LOG` to override it.

See [Direct DRM presentation](docs/drm-presentation.md) for the probe evidence,
ownership boundaries, and production integration constraints.

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
