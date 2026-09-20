# Godot engine issues and preview validation

Status: observed behavior and development workarounds, recorded 2026-09-21.
The XR project currently targets **Godot 4.8-dev6**, engine commit
`8898c2b3db32adf6f92c694ffb6dac19af672e5f`, with matching Android templates.
This is a tested preview, not a claim that all native-window lifetime bugs are
fixed. Compatibility/GLES and native OpenXR composition remain in use.

## Native composition-layer teardown crash

Godot 4.7.1 (`a13da4feb`) crashed on Android's GL thread while retiring native
windows during Azahar/Blender use. The matched engine library had build ID
`4228a4c67b4fccea0941eb9c010b93cf03588cde`. The fault was a null access at
`0xe0`, with PCs `0x237c34c` and `0x237d150` in the render-target cleanup path.
Inspection of the matching GLES code and disassembly traced this to texture
lookup during `_clear_render_target` / `render_target_free`.

The engine's override-texture cache-hit path could replace the render target's
owned texture RID with an external swapchain texture RID. Subsequent cleanup
then treated the wrong texture as owned. Upstream
[Godot PR #122271](https://github.com/godotengine/godot/pull/122271), commit
[`c03267c0a4f77c8e72c6f3ff18ab3a99d4039146`](https://github.com/godotengine/godot/commit/c03267c0a4f77c8e72c6f3ff18ab3a99d4039146),
corrects this ownership path and related render-target initialization/cleanup.
The inspected 4.7.2 source still had the problem; 4.8-dev6 contains the fix.
Weld does not carry a Godot engine patch.

### Isolated reproduction and remaining errors

`scripts/run-godot-xr-teardown` exercises native window creation and retirement
without a decoder, application stream, or network connection. It creates two
colored windows per cycle for 20 cycles, letting swapchain images rotate before
each retirement. It uses the normal device/preparation locks and shared ADB
server, bounds playback to 60 seconds and captured logs to 4 MiB, and removes
its one-shot app-private startup flag during cleanup.

```sh
scripts/run-godot-xr-teardown --serial PICO_SERIAL --mode legacy
scripts/run-godot-xr-teardown --serial PICO_SERIAL --mode delayed --skip-build
```

`legacy` means the ordinary window deletion path, not an old engine selection.
`detach` first clears composition-layer viewports; `delayed` also waits three
process frames before deleting nodes. These are probe-only experiments. No
detach/delay workaround was added to production teardown. `--skip-build` uses
the already installed APK; check its logged engine version before comparing.
The headset must be awake with Weld in the foreground; the probe stops the Weld
Android app before and after its run.

Saved evidence under `target/validation/` (local logs, not tracked artifacts):

| Engine / mode | Run | Result |
| --- | --- | --- |
| 4.7.1 / delayed | `xr-teardown-frmi64h1` | SIGSEGV at `0xe0` on first retirement |
| 4.8-dev6 / legacy | `xr-teardown-6_wvgsys` | 20 cycles / 40 windows; no native crash; 160 `texture_free` and 4532 `texture_proxy_update` records |
| 4.8-dev6 / delayed | `xr-teardown-nn565rcg` | 20 cycles / 40 windows; no native crash; 160 `texture_free` and 4585 `texture_proxy_update` records |

The preview no longer reproduced the specific crash in these bounded checks.
It did **not** make teardown error-free: the probe deliberately returns failure
when those texture-lifetime errors remain, even after its completion marker.
The remaining errors need separate investigation; their effect on long-session
resource use is not established. They are not evidence of codec corruption.

The user subsequently played two Mario 3D Land levels through Azahar on Pico
without significant trouble, after correcting Azahar's circle-pad binding to
direct SDL analog axes. This is useful interactive validation, not a teardown
stress test or proof of long-session stability.

The ordinary live Android hoist launcher does not yet continuously monitor the
viewer process. A successful source-side run/cleanup is therefore not proof
that the Android app survived. Inspect viewer logs and Android exit information;
the isolated teardown runner additionally checks the viewer process.

## Rust binding compatibility

The extension keeps `godot = 0.5.5` and `api-4-7`, adding
`lazy-function-tables`. Godot 4.8 removed unused editor LSP methods, including
`GDScriptTextDocument::foldingRange`, without compatibility bindings in
[PR #114533](https://github.com/godotengine/godot/pull/114533). Eager 4.7 method
table initialization failed before Weld could start. Lazy resolution avoids
looking up unused methods; it does not restore removed APIs or make arbitrary
4.7 API calls compatible with 4.8.

Keep Godot API access on the main thread. Lazy tables are not a reason to enable
threaded Godot access; native render callbacks continue to use native/shared
state rather than looking up Godot methods. Linux and Android debug libraries,
extension startup and shader checks were validated with the preview.

## Launching the preview on NixOS

`scripts/run-godot` selects `WELD_GODOT_BIN`, otherwise an executable
`~/.local/bin/godot`, and runs that local binary through `steam-run`. Without a
local selection it falls back to packaged `godot`; that fallback does not imply
the packaged version contains the teardown fix. All Weld Godot launchers and
engine-version fingerprinting use this wrapper. It preserves the original Rust
tool PATH for the export build hook. Install matching export templates through
the selected editor and refresh the project's Android build template when
changing engine versions. Generated templates, APKs and libraries are untracked.

Godot 4.8's Android export checks require `cmdline-tools/latest`, whereas the Nix
SDK provides a versioned directory. `scripts/godot-android-env.py` creates a
small project-local SDK symlink view under `target/godot-android-env/`, with
`latest` pointing at the highest installed numeric command-line-tools version.
Export uses a private copy of editor settings and matching `ANDROID_HOME` /
`ANDROID_SDK_ROOT` paths. This avoids Gradle/editor SDK disagreement without
modifying the immutable SDK, normal editor settings, or installing SDK packages.

The preview can log this after successful APK export:

```text
ERROR: EditorSettings not instantiated yet when getting setting "export/android/shutdown_adb_on_exit".
```

The Android exporter polling thread reads that setting during shutdown; see the
[pinned exporter code](https://github.com/godotengine/godot/blob/8898c2b3db32adf6f92c694ffb6dac19af672e5f/platform/android/export/export_plugin.cpp).
The launcher accepts only this exact diagnostic from the exact tested dev6
version, after the export completion message, with exit status zero. It remains
visible in logs and produces a console notice. Other errors, versions, steps,
incomplete exports and nonzero exits still fail validation.

The SDK/export run `xr-sdk-launcher-3i8c5bxk` produced an APK; a subsequent run
reused it. Alternating native builds outside and inside Steam's FHS environment
still caused an approximately 40-second native dependency rebuild in one cache
check, despite APK reuse. This build-environment churn remains a performance
follow-up, not a resolved cache claim. The 47 focused launcher/SDK/preflight
tests passed again when recording this checkpoint.
