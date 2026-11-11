"""Build and run Weld Station, or validate it with the same build environment."""

import hashlib
import os
import shutil
import subprocess
import tempfile
import urllib.request
import zipfile
from pathlib import Path
from typing import Annotated

import typer

from dev.build_android_ffmpeg import build as build_ffmpeg
from dev.cli import command
from dev.paths import ROOT

PROJECT = ROOT / "apps/weld-connect"
OUTPUT = ROOT / "target/connect"
# Reuse the validated native renderer's existing optimized dependency artifacts.
CARGO = ROOT / "target/dioxus-texture-probe/cargo"
DX_VERSION = "0.7.10"
DX_SHA256 = "61d2132eaf3cd70ca1ac89a7ecc891fcd770e917a5869e29d465459371b26c83"


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def dioxus_cli():
    directory = ROOT / "target/tools" / ("dioxus-" + DX_VERSION)
    binary = directory / "dx"
    if binary.exists():
        return binary
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / "dx.zip"
    url = f"https://github.com/DioxusLabs/dioxus/releases/download/v{DX_VERSION}/dx-x86_64-unknown-linux-gnu.zip"
    with (
        urllib.request.urlopen(url, timeout=60) as response,
        archive.open("wb") as output,
    ):
        shutil.copyfileobj(response, output)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != DX_SHA256:
        raise RuntimeError("Dioxus CLI checksum mismatch")
    with zipfile.ZipFile(archive) as zipped:
        with zipped.open("dx") as source, binary.open("wb") as output:
            shutil.copyfileobj(source, output)
    binary.chmod(0o755)
    return binary


def main(
    android: Annotated[bool, typer.Option("--android")] = False,
    serial: Annotated[str | None, typer.Option("--serial")] = None,
    no_build: Annotated[bool, typer.Option("--no-build")] = False,
    build_only: Annotated[bool, typer.Option("--build-only")] = False,
    jobs: Annotated[int, typer.Option("--jobs")] = 8,
    check: Annotated[bool, typer.Option("--check")] = False,
    test: Annotated[bool, typer.Option("--test")] = False,
    clippy: Annotated[bool, typer.Option("--clippy")] = False,
):
    if sum((check, test, clippy)) > 1:
        raise typer.BadParameter("choose only one of --check, --test, --clippy")
    validation = "check" if check else "test" if test else "clippy" if clippy else None
    if jobs < 1:
        raise typer.BadParameter("jobs must be positive")
    if serial and not android:
        raise typer.BadParameter("--serial requires --android")
    if validation and (no_build or build_only or serial):
        raise typer.BadParameter(
            "validation options cannot be combined with launch/build-only options"
        )
    if android and validation == "test":
        raise typer.BadParameter(
            "--test runs on Linux; use --android --check or --android --clippy for Android"
        )
    os.umask(0o077)
    OUTPUT.mkdir(parents=True, exist_ok=True)
    env = dict(
        os.environ,
        CARGO_TARGET_DIR=str(CARGO),
        WGPU_BACKEND="gl",
        WGPU_VALIDATION="0",
        WGPU_DEBUG="0",
        DIOXUS_CLI_ENABLED="true",
    )
    manifest = PROJECT / "Cargo.toml"
    if android:
        apk = OUTPUT / "weld-connect.apk"
        if not no_build:
            prefix = build_ffmpeg()
            ndk = Path(
                os.environ.get("ANDROID_NDK_HOME") or os.environ["ANDROID_NDK_ROOT"]
            )
            toolchain = ndk / "toolchains/llvm/prebuilt/linux-x86_64"
            env.update(
                FFMPEG_DIR=str(prefix),
                PKG_CONFIG_LIBDIR="",
                PKG_CONFIG_PATH="",
                BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android=f"--sysroot={toolchain}/sysroot --target=aarch64-linux-android28",
            )
            run(
                "cargo",
                "ndk",
                "-t",
                "arm64-v8a",
                "--platform",
                "28",
                validation or "build",
                "--lib",
                "--locked",
                "--manifest-path",
                manifest,
                "-j",
                jobs,
                env=env,
                cwd=ROOT,
            )
            if validation:
                return
            library = CARGO / "aarch64-linux-android/debug/libweld_connect.so"
            packaged = OUTPUT / "libweld_connect.so"
            shutil.copy2(library, packaged)
            assets = OUTPUT / "android-assets"
            run(dioxus_cli(), "tools", "assets", packaged, assets, cwd=PROJECT)
            sdk = Path(os.environ.get("ANDROID_HOME") or os.environ["ANDROID_SDK_ROOT"])
            ndk = Path(
                os.environ.get("ANDROID_NDK_HOME") or os.environ["ANDROID_NDK_ROOT"]
            )
            tools = sdk / "build-tools/36.1.0"
            run(
                ndk / "toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip",
                "--strip-debug",
                packaged,
            )
            native = OUTPUT / "native/arm64-v8a"
            native.mkdir(parents=True, exist_ok=True)
            shutil.copy2(packaged, native / packaged.name)
            for name in ("avcodec", "avformat", "avutil"):
                shutil.copy2(prefix / f"lib/lib{name}.so", native / f"lib{name}.so")
            shutil.copy2(PROJECT / "wrap.sh", native / "wrap.sh")
            run(
                "java",
                "-classpath",
                ROOT / "apps/weld-mobile/android/gradle/wrapper/gradle-wrapper.jar",
                "org.gradle.wrapper.GradleWrapperMain",
                "--no-daemon",
                "--console=plain",
                "-p",
                PROJECT / "android",
                f"-Pandroid.aapt2FromMavenOverride={tools / 'aapt2'}",
                "assembleDebug",
                cwd=ROOT,
            )
            unsigned = OUTPUT / "unsigned.apk"
            shutil.copy2(
                OUTPUT / "android/outputs/apk/debug/weld-connect-debug-unsigned.apk",
                unsigned,
            )
            # Gradle's JNI packaging selects shared libraries; Android's debug
            # wrapper must be added explicitly before alignment and signing.
            with zipfile.ZipFile(
                unsigned, "a", compression=zipfile.ZIP_STORED
            ) as output:
                if "lib/arm64-v8a/wrap.sh" not in output.namelist():
                    output.write(PROJECT / "wrap.sh", "lib/arm64-v8a/wrap.sh")
            aligned = OUTPUT / "aligned.apk"
            run(tools / "zipalign", "-f", "-P", "16", "4", unsigned, aligned)
            local = PROJECT / ".local"
            local.mkdir(mode=0o700, exist_ok=True)
            key = local / "debug.keystore"
            if not key.exists():
                run(
                    "keytool",
                    "-genkeypair",
                    "-keystore",
                    key,
                    "-storepass",
                    "android",
                    "-keypass",
                    "android",
                    "-alias",
                    "androiddebugkey",
                    "-keyalg",
                    "RSA",
                    "-keysize",
                    "2048",
                    "-validity",
                    "10000",
                    "-dname",
                    "CN=Weld Connect Debug",
                )
            run(
                tools / "apksigner",
                "sign",
                "--ks",
                key,
                "--ks-pass",
                "pass:android",
                "--out",
                apk,
                aligned,
            )
            run(tools / "apksigner", "verify", apk)
        if not build_only:
            adb = ["adb", *(["-s", serial] if serial else [])]
            run(*adb, "install", "--no-incremental", "-r", apk)
            run(*adb, "shell", "am", "force-stop", "org.weld.connect")
            run(
                *adb,
                "shell",
                "am",
                "start",
                "-n",
                "org.weld.connect/org.weld.connect.WeldActivity",
            )
    else:
        libraries = [
            subprocess.check_output(
                ["pkg-config", "--variable=libdir", name], text=True
            ).strip()
            for name in (
                "egl",
                "glesv2",
                "libavcodec",
                "libavutil",
                "libavformat",
                "libavfilter",
                "libva",
                "libva-drm",
            )
        ]
        env["LD_LIBRARY_PATH"] = ":".join(
            dict.fromkeys([*env.get("LD_LIBRARY_PATH", "").split(":"), *libraries])
        ).strip(":")
        if validation:
            run(
                "cargo",
                validation,
                "--all-targets",
                "--locked",
                "--manifest-path",
                manifest,
                "-j",
                jobs,
                env=env,
                cwd=ROOT,
            )
            return
        binary = OUTPUT / "weld-connect"
        if not no_build:
            run(
                "cargo",
                "build",
                "--locked",
                "--manifest-path",
                manifest,
                "--bin",
                "weld-connect",
                "-j",
                jobs,
                env=env,
                cwd=ROOT,
            )
            with tempfile.TemporaryDirectory(prefix="bundle-", dir=OUTPUT) as temporary:
                prepared = Path(temporary) / "weld-connect"
                shutil.copy2(CARGO / "debug/weld-connect", prepared)
                run(
                    dioxus_cli(),
                    "tools",
                    "assets",
                    prepared,
                    OUTPUT / "assets",
                    cwd=PROJECT,
                )
                os.replace(prepared, binary)
        if not build_only:
            run(binary, env=env, cwd=ROOT)


app = command(main, help=__doc__)


if __name__ == "__main__":
    app()
