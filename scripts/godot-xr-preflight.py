"""Local successful-preparation cache and bounded validation lanes, not a build system.

Cargo still checks/builds both native targets before consulting this cache.
Only hashes are persisted; settings, environment and signing secrets are not.
"""
from concurrent.futures import ThreadPoolExecutor, as_completed, wait
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from threading import Event


def file_hash(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def fingerprint(root, project, environment):
    inputs, metadata = hashlib.sha256(), hashlib.sha256()
    prefixes = ("RUST", "CARGO", "ANDROID", "NDK", "JAVA", "GODOT", "PKG_CONFIG",
                "BINDGEN", "CC", "CXX", "AR", "CFLAGS", "CPPFLAGS", "LDFLAGS",
                "LIBGL", "MESA", "VK_", "HOST_", "TARGET_", "NIX_", "CLIPPY",
                "LIBCLANG", "CRATE_CC", "WELD_RUST_BUILD_PATH")
    names = {"PATH", "HOME", "LD_LIBRARY_PATH", "LIBRARY_PATH", "FFMPEG_DIR", "DISPLAY", "WAYLAND_DISPLAY",
             "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"}
    relevant = {key: value for key, value in environment.items() if key in names or key.startswith(prefixes)}
    inputs.update(json.dumps(relevant, sort_keys=True).encode())
    for command in (["cargo", "--version"], ["rustc", "-Vv"], ["godot", "--version"]):
        executable = shutil.which(command[0], path=environment.get("PATH"))
        if executable is None:
            raise RuntimeError(f"missing {command[0]} while checking preparation freshness")
        inputs.update(str(Path(executable).resolve()).encode())
        inputs.update(subprocess.check_output(command, cwd=root, env=environment, timeout=20))

    sources = ["apps/weld-vr", "crates", "vendor", ".cargo", "Cargo.toml", "Cargo.lock",
               "rust-toolchain", "rust-toolchain.toml", "rustfmt.toml", ".rustfmt.toml",
               "clippy.toml", ".clippy.toml", "scripts/run-godot-xr", "scripts/godot-xr-preflight.py",
               "scripts/build-android-ffmpeg", "scripts/ffmpeg-patches"]
    listed = subprocess.check_output(["git", "ls-files", "-co", "--exclude-standard", "-z", "--", *sources],
                                     cwd=root, env=environment, timeout=20)
    paths = {root / os.fsdecode(path) for path in listed.split(b"\0") if path}
    template = project / "android/build"
    generated = [project / path for path in (".godot", "bin", "build", "target", "rust/target",
                                             "android/build/build", "android/build/.gradle",
                                             "android/build/assetPackInstallTime/build")]
    # Include local/ignored project resources too, but never traverse build trees.
    for directory, children, files in os.walk(project):
        children[:] = [name for name in children if Path(directory) / name not in generated
                       and name not in (".git", "__pycache__")]
        paths.update(Path(directory) / name for name in files)
    paths = {path for path in paths if not any(path.is_relative_to(output) for output in generated)}
    paths.update((project / "bin").glob("*/debug/*.so"))
    # Missing import state must invalidate cached engine probes after a clean.
    paths.update(project / path for path in (".godot/extension_list.cfg", ".godot/imported"))

    home = Path(environment.get("HOME", str(Path.home())))
    cargo_home = Path(environment.get("CARGO_HOME", str(home / ".cargo")))
    paths.update(cargo_home / name for name in ("config", "config.toml"))
    config = Path(environment.get("XDG_CONFIG_HOME", str(home / ".config"))) / "godot"
    data = Path(environment.get("XDG_DATA_HOME", str(home / ".local/share"))) / "godot"
    paths.update((data / "export_templates").glob("*/android_debug.apk"))
    paths.update([home / ".android/debug.keystore", data / "keystores/debug.keystore"])
    # Only export settings matter, not editor history that Godot rewrites.
    for settings in sorted(config.glob("editor_settings-*.tres")):
        lines = [line for line in settings.read_text().splitlines() if line.startswith("export/android/")]
        inputs.update(json.dumps([str(settings), lines]).encode())
        for line in lines:
            match = re.match(r'export/android/debug_keystore = (".*")$', line)
            if match:
                paths.add(Path(json.loads(match[1])).expanduser())
    # Custom debug templates/keys can live outside the project.
    for settings in (project / "export_presets.cfg", project / "export_credentials.cfg"):
        if settings.is_file():
            for match in re.finditer(r'^(?:custom_template/debug|keystore/debug)=(".*")$', settings.read_text(), re.M):
                value = json.loads(match[1])
                if value:
                    paths.add(project / value[6:] if value.startswith("res://") else Path(value).expanduser())
    for path in sorted(paths):
        engine_written = path.suffix in (".uid", ".import") or path.is_relative_to(project / ".godot")
        # These are regenerated by a debug Gradle export. They still invalidate
        # a cache hit, but cannot be treated as mid-build edits. Template scripts,
        # AARs, Java/Kotlin and the main manifest remain stable source inputs.
        engine_written |= any(path.is_relative_to(template / directory) for directory in
                              ("src/main/assets", "res"))
        engine_written |= path in (template / "src/debug/AndroidManifest.xml", template / "libs/gdextensionlibs.json")
        engine_written |= path.is_relative_to(template / "libs") and path.suffix == ".so"
        digest = metadata if engine_written else inputs
        digest.update(os.fsencode(path) + b"\0" + os.fsencode(path.resolve()) + b"\0")
        if path.is_file():
            digest.update(file_hash(path).encode())
        else:
            digest.update(b"directory" if path.is_dir() else b"missing")
    return {"inputs": inputs.hexdigest(), "metadata": metadata.hexdigest()}


def cache_matches(path, key, apk):
    try:
        saved = json.loads(path.read_text())
        return isinstance(saved, dict) and saved.get("key") == key and saved.get("apk") == file_hash(apk)
    except (OSError, ValueError):
        return False


def save_cache(path, key, apk):
    descriptor, temporary = tempfile.mkstemp(prefix="xr-preflight-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as output:
            json.dump({"key": key, "apk": file_hash(apk)}, output)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def parallel_checks(groups, runner, directory, environment):
    cancelled = Event()

    def lane(steps):
        for step in steps:
            if cancelled.is_set():
                return
            runner(step, directory, environment, cancelled=cancelled)

    executor = ThreadPoolExecutor(max_workers=2)
    futures = []
    try:
        futures = [executor.submit(lane, steps) for steps in groups]
        for future in as_completed(futures):
            future.result()
    finally:
        cancelled.set()
        _, pending = wait(futures, timeout=12)
        # Always join before releasing locks, even if cleanup overruns its
        # expected window. Report an overrun only after joining; never abandon
        # threads owning processes merely to meet a timeout.
        executor.shutdown(wait=True, cancel_futures=True)
        if pending:
            raise RuntimeError("XR check cleanup exceeded its 12-second deadline")
