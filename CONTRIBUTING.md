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

`scripts/check-host-runtime` is a bounded real-Wayland regression runner
(Python 3, supplied by the development shell). Use `--shm-only` to exercise
hosting without Vulkan, or `--nested` to check native screenshot capture with
foot on an existing desktop. The default protocol fixture installs an explicit
test presentation consumer. `--stalled-presenter --shm-only` verifies it can
progress even when a fake native display never completes a frame;
`--dormant --shm-only` checks the production headless host without any viewer.
`--reclaim-presenter --shm-only` checks that returning callback ownership
restarts local composition without another client commit or UI redraw.
`--policy-only --shm-only` also checks non-Bevy
policy servicing with no presenter or rendering. Python provides process-group ownership
and bounded waits for these subprocess tests. Logs remain in `target/validation`.
Add `--frame-timings` to a protocol-probe run to distinguish frame-callback
delay from SHM buffer-release delay; this is not an encoder or GPU latency test.

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

Hardware media work requires FFmpeg development/runtime libraries, libva
development/runtime libraries, `libva-utils`, Clang, and bindgen. In
particular, `libavcodec`, `libavfilter`, `libavformat`, and `libavutil` must be
visible to `pkg-config`; they are required by ordinary workspace checks because
the standard distribution includes encoded hoisting. On NixOS, the shared Rust
shell provides those libraries and points libva at the active system driver
directory under `/run/opengl-driver/lib/dri`; it deliberately does not force a
vendor driver name. A missing FFmpeg or libva environment must be reported
separately from a loaded driver that lacks a requested capability.

Run Weld inside a development shell whose glibc is compatible with the running
NixOS graphics drivers. The shared Rust shell is located at
`/home/jonas/Dotfiles/nixos/envs/rust`; reload it after its lock file changes.

Launch Weld's nested development host without a client:

```text
cargo run -- --backend nested --config examples/master.sway.config
```

Backend selection defaults to `auto`. Use an explicit backend when validating
host-specific behavior so the environment cannot change the target silently.

`--backend headless` runs the presentation-free Wayland session host. It has
configurable virtual output and initial window sizes. For a headless source
streamed to a nested receiver, run `scripts/run-headless-iroh-hoist`; it starts
foot/htop, Blender and a private-profile Firefox, with whole-session consent.
See [Headless application hosting](docs/headless-host.md) for commands and limits.
`auto` never selects it.

Pass a program and arguments to launch it against Weld's private Wayland
socket. The verified smoke test uses foot:

```text
cargo run -- --backend nested --config examples/master.sway.config -- foot
```

To open another application in an already-running Weld instance, use the
client launcher from a second shell. It connects the application to Weld's
`weld-0` Wayland socket and does not start another compositor:

```text
scripts/run-app foot
```

The launcher forces common toolkits onto their native Wayland backends and
disables X11 fallback.

For X11 clients, enable `--xwayland` and launch through Weld (CLI, config `exec`,
or a terminal inside Weld), which supplies the session's private `DISPLAY`.
See [Rootless XWayland](docs/xwayland.md) for validation commands and limits.

The development example config provides these backend-neutral shortcuts:

- `Alt+Enter`: launch foot
- `Alt+Space`: launch Rofi (`drun`)
- `Alt+Control+Shift+F`: launch Firefox
- `Alt+B`: launch Blender
- `Alt+H`: hoist or locally preview the focused occupied window
- `Alt+D/F/K/J` or `Alt+Arrow`: focus left/right/up/down
- Add `Shift` to navigate a tile structurally through neighboring splits
- `Alt+Control+Arrow`: adjust tile proportions
- `Alt+Control+F/J`: prepare a horizontal/vertical split
- `Alt+Shift+R`: reload Master configuration
- `Alt+Shift+Q`: request close of the focused client
- `Alt+Shift+O`: toggle output-topology diagnostics
- `Alt+Shift+Escape`: exit Weld

These example bindings live in `examples/master.sway.config`, not the reusable plugins.
That file configures Ctrl/Caps and left Alt/Windows swaps: logical Alt is the
physical Windows key with those settings. Master reads explicit Weld input
configuration in both nested and DRM modes.
The example starts Waybar through `exec`, using the repository's example config
and style paths. Launch from the repository root, or adjust those paths in your
own configuration. Its workspace buttons use `ext-workspace-v1`.
Master now uses the native tiler. Its initial Sway configuration subset and
reload semantics are documented in [Master tiling](docs/master-tiling.md).

Weld options precede an explicit `--` when a client is also present. Capture a
settled client-plus-shell composition and exit with:

```text
cargo run -- --config examples/master.sway.config --screenshot target/weld-startup.png -- foot
```

Enable the restricted, loopback-only development protocol with:

```text
cargo run -- --config examples/master.sway.config --remote-debug -- foot
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

Use the Smithay output-compositor probe when changing the production DRM
boundary:

```text
scripts/run-smithay-drm-compositor-probe
```

Run the production startup-output backend from a real TTY with:

```text
scripts/run-weld-drm --config examples/master.sway.config
scripts/run-weld-drm --seconds 30 --config examples/master.sway.config -- foot
WELD_DRM_VALIDATE=1 scripts/run-weld-drm --config examples/master.sway.config -- foot
WELD_DRM_PACING_TRACE=1 scripts/run-weld-drm --config examples/master.sway.config -- foot
```

`--seconds` starts its watchdog after compilation and sends Weld `SIGTERM` when
the interval expires, allowing its normal DRM shutdown to restore the console.
The first command exercises ordinary validation. The environment flag enables
the Khronos validation layer and synchronization validation for a focused GPU
correctness run. `WELD_DRM_PACING_TRACE=1` records one diagnostic event per
queued frame and vblank, including cursor-plane assignment, vblank sequence
deltas, composition state, and GPU wait time. Compare behavior with a normal
run because writing the trace can itself perturb frame timing. Output defaults
to `target/validation/weld-drm.log`.

The probes and production DRM backend require a real TTY. See
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

Write comments in positive, concrete terms: explain purpose, behavior, invariants
and the reason for a decision. Avoid boilerplate about what something "isn't"
or lists of systems it does not depend on. Keep architectural comparisons and
scope discussions in the relevant design documentation.

## Commits

Make an atomic commit after each coherent batch. Use a short subject line and
omit the body unless it adds essential context. Never add generated-by or
co-author attribution trailers.
