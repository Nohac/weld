# Weld VR

Godot shell for Weld's VR client, initially developed on an Android phone.
Keep the Godot project and Rust GDExtension together here; reuse existing
Weld crates rather than duplicating protocol, transport or media logic.

The project uses the Mobile renderer. Godot XR Tools is enabled, including its
user-settings and rumble-manager autoloads. OpenXR startup is not enabled yet:
phone-first setup does not require a headset runtime. Headset support must use
standard OpenXR, without a Pico SDK, Pico XR plugin or developer-account login.

## Godot XR Tools

Vendored under `addons/godot-xr-tools/`. Upstream code and assets are unchanged;
Godot 4.7 has updated the tracked `.import` metadata.

- Release: [4.5.1](https://github.com/GodotVR/godot-xr-tools/releases/tag/4.5.1),
  the latest stable release checked on 2026-09-11. This is the toolkit version,
  not the Godot engine version (4.7.1 in the Android development shell).
- Archive: [godot-xr-tools.zip](https://github.com/GodotVR/godot-xr-tools/releases/download/4.5.1/godot-xr-tools.zip).
- SHA-256: `f60d15e6b1bc4e544947691cb8de73c483dfe9e9a4c4dad92511e0d8f575dcae`.
- License: [MIT](addons/godot-xr-tools/LICENSE), with upstream asset notices
  retained in the add-on.

Only the add-on directory is installed, not the demo project or vendor plugins.
For updates, use an explicit upstream stable release, verify its archive digest,
and review the replacement before importing it with Godot. Keep upstream files
unchanged; put Weld behavior outside the add-on.

Open this directory from the Android development shell. If the editor was open
during installation, reload the project so it discovers the add-on's classes and
autoloads. See the shared shell's README for SDK/NDK and export-template setup.

## Rust bridge

`rust/` is a small, independent Cargo workspace, pinned to `godot` 0.5.5 with
Godot 4.7 API bindings. It does not join the Linux compositor workspace or change
its lockfile. The reduced binding set is sufficient for our `WeldBridge` node;
GDScript owns the button and label. Rust owns the response counter and logs node
entry/exit and application pause/resume. There is no per-frame polling, worker,
networking, media decoder or XR runtime integration in this slice.

This follows the working
[Android build comment](https://github.com/godot-rust/gdext/issues/470#issuecomment-4587348846):
an ARM64 `cdylib`, `cargo-ndk`, Android library mappings, and ordinary Godot APK
export. The existing Nix environment supplies the compatible SDK/NDK/JDK; do not
reinstall the comment's example SDK versions or generate another signing key.
Unlike its example `../target` paths, our staged libraries stay inside `res://bin`
as required by the current [gdext export guidance](https://godot-rust.github.io/book/intro/hello-world.html#the-gdextension-file).

### Build and run

From the Weld repository root, in the shared Rust development shell:

```sh
# Linux editor library only:
apps/weld-vr/scripts/build-gdextension

# Linux editor library plus Android ARM64 library:
apps/weld-vr/scripts/build-gdextension android

godot --editor --path apps/weld-vr
```

Build before opening a fresh checkout, so Godot can register the native class.
The helper uses locked dependencies, at most two Cargo jobs and debug builds.
Cargo outputs stay under `rust/target/`; libraries are atomically staged under
`bin/`, so rebuilding does not truncate a library mapped by the editor. Both
directories are ignored by Git, and `.gdignore` keeps Cargo sources and outputs
out of Godot's resource import. The main scene keeps its UID reference so Godot
can track scene moves and renames.

After the initial build, the **Weld Rust Build** editor addon runs the same helper
automatically: desktop Play builds Linux, and Android debug export/Run on Device
builds Android ARM64 plus the editor library. Cargo decides what needs rebuilding;
unchanged libraries are not re-staged, avoiding needless hot reloads. Android
export is checked even when Deploy with Remote Debug is off. Other export
platforms are untouched. No compilation happens merely from opening the editor.
Exporting a PCK/ZIP with the Android preset runs the same Android build hook.

Start Godot from the shared Rust development shell so its child Cargo process
inherits the toolchain environment. Builds are synchronous: the editor waits,
then prints compiler output in the Output panel. Unsupported Android profiles
or architectures produce an export error instead of attempting a different build.

A failed desktop build stops Play. A failed Android build adds an **export error**
with its exit code and compiler diagnostics, but Godot 4.7.1 may still install
the last successful library. The native deploy result dialog reports this after
the run. Fix the error and deploy again; do not interpret the old app launching
as a successful rebuild. With remote debugging on, a preliminary desktop build
failure also does not stop Android deployment; the Android export hook reports
the failure again. The addon never deletes the last good library.

This is a Godot API limitation: `_export_begin` cannot return failure, and native
deployment does not propagate `_build` failure. CLI export can likewise exit 0
despite the plugin error. For automation that requires a hard failure, run
`build-gdextension android` successfully **before** invoking Godot export.

Only Linux x86_64 and Android ARM64 **debug** mappings are provided. Release
exports and other architectures are not supported by this bootstrap.

### Phone validation

After the initial build, use the existing **Android Phone** preset and
Godot's debug export/one-click deployment, just as for the original Hello World.
Godot can use its existing default debug keystore; no new manual key setup is
needed. Never use that development key for production releases.

1. Launch the app and tap **Call Rust**. The label should show `Hello from Rust!`
   and `Tap count: 1`; further taps increment the count.
2. Background and resume the app, then tap again. If Android retained the process,
   the count should continue. Rust logs the pause/resume notifications.
3. Close and relaunch the app. A new bridge starts its count at zero.
4. Inspect `adb logcat -s godot` for the `weld-vr:` lifecycle and button messages.

The build helper never installs, launches or stops an app on a device. Godot
4.7.1 exposes APK export through its CLI, but not the editor's combined one-click
deploy action; command-line deployment uses export, `adb install -r`, and
`adb shell am start` separately.

Validated on a Pixel 8 Pro on 2026-09-11: the debug APK updated the existing app
using its existing debug key, launched with the Mobile Vulkan renderer, loaded
the ARM64 Rust bridge, and displayed incrementing responses to touch input.
The user confirmed the interaction worked. Android background/resume and
headset behavior still need separate validation.

### Automated checks

```sh
apps/weld-vr/scripts/check-gdextension
# Also exercise real unsigned APK exports with a simulated compiler:
apps/weld-vr/scripts/check-gdextension --android-export
cargo fmt --manifest-path apps/weld-vr/rust/Cargo.toml --check
cargo clippy --manifest-path apps/weld-vr/rust/Cargo.toml \
  --locked --jobs 2 --all-targets -- -D warnings
```

The integration check builds the desktop library and imports a temporary project
copy with isolated Godot settings. Its first import omits the main-scene setting
only in that copy to bootstrap the UID database, then restores the original
configuration and loads the scene through its UID. It tests the real scene's
button-to-Rust-to-label connection and fresh state after scene recreation,
checking both process status and Godot error output. Temporary copies are
removed on exit; your editor cache, export credentials and connected devices
are not used.

The check also exercises target selection and compiler diagnostics with a helper
fixture. The optional Android export checks use the installed matching export
template and SDK/JDK, no signing key or device. They export the main scene,
autoload dependencies and GDExtension, avoiding unrelated unused XR scenes.
They verify both normal export and the expected soft failure with a good library
already staged: the error must be logged, the library preserved, and editor/test
scripts excluded. These checks do not exercise the editor's device-result dialog.

Desktop and ARM64 builds, Clippy and the headless integration check pass with
Godot 4.7.1 and Rust 1.95. An exported Android debug APK contains the exact staged
ARM64 `libweld_vr.so`, including `gdext_rust_init`, and no desktop copy of that
library. The root Weld Cargo manifest, lockfile and build configuration are
unchanged.

For headless APK export, import resources before exporting. A combined first
import/export produced errors from unused XR Tools resources and exit-time
resource cleanup; the same errors were reproduced from the pre-Rust checkpoint
`d89f0aab`. The separate import and subsequent export used for the phone build
completed without those errors. This does not validate the unused XR scenes.
