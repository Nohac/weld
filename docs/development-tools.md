# Development tools

Run these from the normal Rust development shell so native build dependencies
remain available. The root `pyproject.toml` defines named console commands;
each entry points directly to a Typer application. `uv.lock` pins the environment.

```sh
uv sync --locked
uv run check-desktop-build-graph
uv run check-config-generation
uv run check-host-runtime --shm-only
uv run run-connect --check
uv run weld-debug --help
```

All commands share `tools/dev/cli.py`. Its single `FANCY_OUTPUT = False`
setting disables Rich help, ANSI colors and pretty tracebacks. Change it there
to enable those features for every command. Commands do not duplicate formatting
configuration. `--help` is available without opening devices or starting builds.

## Maintained commands

| Commands | Responsibility |
| --- | --- |
| `generate-config`, `import-i3-config`, `check-config-generation` | Grammar import, Rust generation and ownership checks |
| `check-desktop-build-graph` | Cargo dependency-feature parity without compilation |
| `check-host-runtime`, `check-control-launch` | Isolated host and control regression fixtures |
| `check-output-capture`, `check-screenshot-capture`, `check-window-recording` | Capture and recording regression fixtures |
| `check-layer-shell`, `check-xwayland` | Focused shell/X11 integration checks |
| `paced-render` | Bounded headless compositor CPU benchmark |
| `build-android-ffmpeg`, `build-mobile`, `run-connect` | Current application builds and Weld Station launch |
| `run-godot` | Select Godot while preserving the Rust build environment |
| `weld-debug` | Restricted development RPC client |

Nested/interactive checks retain their existing presentation requirements;
`check-output-capture` defaults to headless Weston. CLI validation tests do not
launch any compositor or access a GPU. `paced-render` is explicitly a performance
workflow and builds its benchmark in the release profile.

The portal fixture uses native PyGObject/GStreamer from the development
installation rather than installing native GI packages into uv. Set
`WELD_TEST_PORTAL_PYTHON` explicitly when needed; otherwise it finds `python3`
outside the uv virtual environment. Portal frontend/backend paths are still
selected through `WELD_TEST_PORTAL_FRONTEND` and `WELD_TEST_PORTAL_BACKEND`.

Godot's two application-owned shell hooks under `apps/weld-vr/scripts` remain
because the editor/export integration invokes them directly. Their shared Python
build/launch dependencies now use the root uv commands. `scripts/` otherwise
contains only internal fixtures and native patches, not public command wrappers.

## Validation

```sh
uv run python -m unittest discover -s tools/dev/tests -v
uv run ruff check tools/dev
uv run ruff format --check tools/dev
uv run ty check
uv run check-config-generation
uv run check-desktop-build-graph
```

Ruff and ty are locked dev dependencies. ty checks the importable development
package; the optional native GI portal fixture runs under its separately selected
system interpreter.

The tests cover registered Typer entry points, plain help/errors, the shared
formatting toggle, invalid combinations before external work, Godot argument
forwarding, build-only validation and grammar generation. Hardware and device
workflows remain opt-in; changing their CLI does not validate a fresh Android
build, an XR device, or every native presentation path.

## Removed in the cleanup

- Standalone Dioxus texture/stream, Android codec, Iroh ADB, network-hoist and
  OpenXR alignment probe projects, with their launchers and probe-only tests.
- Godot/mobile/XR demo orchestration, Azahar launch presets, preflight caches,
  chart/report generators and ADB helpers used exclusively by those demos.
- Old local/Iroh/network hoist demos and the large perf/Tracy/orbit scenario
  wrappers. The maintained paced-render benchmark and its native fixtures remain.
- `run-app`, superseded by `weldctl launch`; the root crash-log launcher;
  DRM example/debug wrappers, superseded by explicit Cargo commands.
- The separate remote-debug Python project, now part of the root uv package.

Historical reports retain their measurements and identify retired commands.
Recover a retired workflow from Git if it becomes useful again. Personal
untracked files and ignored build caches are not deleted.

The four remaining vendors are active: Smithay, Bevy render/UI, and the patched
AnyRender implementation used by Weld Station. Removing them requires replacing
or upstreaming their current behavior; they are not unused cleanup candidates.
