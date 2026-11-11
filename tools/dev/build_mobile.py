"""Build/sign the Android ARM64 debug receiver with the SDK from the dev shell."""

import os
import shutil
import subprocess
from pathlib import Path
from typing import Annotated

import typer

from dev.build_android_ffmpeg import build as build_ffmpeg
from dev.cli import command
from dev.paths import ROOT

OUTPUT = ROOT / "target/mobile"
PROJECT = ROOT / "apps/weld-mobile"


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def main(
    install: Annotated[bool, typer.Option("--install")] = False,
    serial: Annotated[
        str | None,
        typer.Option(
            "--serial",
            help="ADB device serial (required when multiple devices are attached)",
        ),
    ] = None,
    jobs: Annotated[int, typer.Option("--jobs")] = 8,
    clippy: Annotated[
        bool,
        typer.Option(
            "--clippy", help="check Android code without building/deploying an APK"
        ),
    ] = False,
):
    if jobs < 1:
        raise typer.BadParameter("--jobs must be positive")
    if clippy and install:
        raise typer.BadParameter("--clippy cannot be combined with --install")
    os.umask(0o077)
    sdk = Path(os.environ.get("ANDROID_HOME") or os.environ["ANDROID_SDK_ROOT"])
    ndk = Path(os.environ.get("ANDROID_NDK_HOME") or os.environ["ANDROID_NDK_ROOT"])
    tools = sdk / "build-tools/36.1.0"
    platform = sdk / "platforms/android-36/android.jar"
    if not platform.is_file() or not (tools / "aapt2").is_file():
        raise typer.BadParameter(
            "Android platform 36 and build-tools 36.1.0 are required"
        )
    OUTPUT.mkdir(parents=True, exist_ok=True)
    prefix = build_ffmpeg()
    toolchain = ndk / "toolchains/llvm/prebuilt/linux-x86_64"
    env = dict(
        os.environ,
        CARGO_TARGET_DIR=str(OUTPUT / "cargo"),
        ANDROID_NDK_HOME=str(ndk),
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
        "clippy" if clippy else "build",
        "--manifest-path",
        PROJECT / "Cargo.toml",
        "--lib",
        "--locked",
        "-j",
        jobs,
        *(["--", "-D", "warnings"] if clippy else []),
        env=env,
        cwd=ROOT,
    )
    if clippy:
        return
    library = OUTPUT / "cargo/aarch64-linux-android/debug/libweld_mobile.so"
    native = OUTPUT / "native/arm64-v8a"
    native.mkdir(parents=True, exist_ok=True)
    for source in [
        library,
        *(prefix / f"lib/lib{name}.so" for name in ("avcodec", "avformat", "avutil")),
    ]:
        destination = native / source.name
        if destination.is_symlink() and destination.resolve() == source.resolve():
            continue
        if destination.is_symlink():
            destination.unlink()
        destination.symlink_to(source)
    run(
        "java",
        "-classpath",
        PROJECT / "android/gradle/wrapper/gradle-wrapper.jar",
        "org.gradle.wrapper.GradleWrapperMain",
        "--no-daemon",
        "--console=plain",
        "-p",
        PROJECT / "android",
        f"-Pandroid.aapt2FromMavenOverride={tools / 'aapt2'}",
        "assembleDebug",
        cwd=ROOT,
    )
    unsigned = OUTPUT / "android/outputs/apk/debug/weld-mobile-debug-unsigned.apk"
    run(tools / "zipalign", "-f", "-P", "16", "4", unsigned, OUTPUT / "aligned.apk")
    # An isolated debug key leaves the user's existing Android signing setup alone.
    local = PROJECT / ".local"
    local.mkdir(mode=0o700, exist_ok=True)
    key = local / "debug.keystore"
    previous_key = OUTPUT / "debug.keystore"
    if not key.exists() and previous_key.exists():
        shutil.copy2(previous_key, key)
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
            "CN=Weld Debug",
        )
    apk = OUTPUT / "weld-mobile.apk"
    run(
        tools / "apksigner",
        "sign",
        "--ks",
        key,
        "--ks-pass",
        "pass:android",
        "--out",
        apk,
        OUTPUT / "aligned.apk",
    )
    run(tools / "apksigner", "verify", apk)
    print(f"Debug APK: {apk}", flush=True)
    if install:
        adb = ["adb"] + (["-s", serial] if serial else [])
        run(*adb, "install", "-r", apk)
        run(
            *adb,
            "shell",
            "am",
            "start",
            "-n",
            "org.weld.mobile/org.weld.mobile.WeldActivity",
        )


app = command(main, help=__doc__)


if __name__ == "__main__":
    app()
