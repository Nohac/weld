"""Project-local compatibility view for Godot's Android SDK directory checks.

Nix supplies versioned command-line tools; Godot 4.8 requires a `latest` entry.
Neither the immutable SDK nor the user's editor settings are modified. Export
uses a private settings copy and a small symlink tree under the build directory.
"""
import hashlib
import json
from pathlib import Path
import re


def link(path, target):
    if path.is_symlink() and path.resolve() == target.resolve():
        return
    if path.exists() or path.is_symlink():
        raise RuntimeError(f"unexpected SDK-view entry; refusing to replace {path}")
    path.symlink_to(target, target_is_directory=target.is_dir())


def export_environment(root, environment):
    selected = environment.get("ANDROID_HOME") or environment.get("ANDROID_SDK_ROOT")
    if not selected:
        return environment
    sdk = Path(selected).resolve()
    if not sdk.is_relative_to("/nix/store") or (sdk / "cmdline-tools/latest").is_dir():
        return environment
    versions = [path for path in (sdk / "cmdline-tools").iterdir()
                if re.fullmatch(r"\d+(?:\.\d+)*", path.name) and (path / "bin/sdkmanager").is_file()]
    if not versions:
        raise RuntimeError("Nix SDK has no installed command-line tools to alias as latest")
    tools = max(versions, key=lambda path: tuple(map(int, path.name.split("."))))
    config = Path(environment.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    identity = hashlib.sha256(f"{sdk}\n{tools}\n{config}".encode()).hexdigest()[:16]
    cache = root / "target/godot-android-env" / identity
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    view = cache / "sdk"
    (view / "cmdline-tools").mkdir(parents=True, exist_ok=True)
    for entry in sdk.iterdir():
        if entry.name != "cmdline-tools":
            link(view / entry.name, entry)
    for entry in (sdk / "cmdline-tools").iterdir():
        link(view / "cmdline-tools" / entry.name, entry)
    link(view / "cmdline-tools/latest", tools)

    settings = sorted((config / "godot").glob("editor_settings-*.tres"))
    if not settings:
        raise RuntimeError("open Godot once to create editor settings before using the Nix SDK view")
    private_config = cache / "config"
    destination = private_config / "godot"
    destination.mkdir(parents=True, exist_ok=True, mode=0o700)
    for source in settings:
        text = source.read_text()
        replacement = "export/android/android_sdk_path = " + json.dumps(str(view))
        pattern = r"^export/android/android_sdk_path = .*?$"
        if re.search(pattern, text, re.M):
            text = re.sub(pattern, lambda _: replacement, text, flags=re.M)
        elif "[resource]" in text:
            text = text.replace("[resource]", "[resource]\n" + replacement, 1)
        else:
            raise RuntimeError(f"editor settings have no resource section: {source}")
        target = destination / source.name
        if not target.exists() or target.read_text() != text:
            target.write_text(text)
            target.chmod(0o600)
    return dict(environment, ANDROID_HOME=str(view), ANDROID_SDK_ROOT=str(view),
                XDG_CONFIG_HOME=str(private_config))
