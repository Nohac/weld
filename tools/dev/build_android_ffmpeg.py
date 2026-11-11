"""Build pinned FFmpeg libraries for the Android applications (ARM64/API 28)."""

import fcntl
import hashlib
import os
import re
import shutil
import signal
import subprocess
import tarfile
import tempfile
from pathlib import Path
from typing import Annotated

import typer

from dev.cli import command
from dev.paths import ROOT

REVISION = "38b88335f99e76ed89ff3c93f877fdefce736c13"
CHECKSUM = "2ae7e42343cfffb811d15cfe98b6d005f082595fcdf034d30a4ff90cfed9f9c6"
PATCH = ROOT / "scripts/ffmpeg-patches/mediacodec-low-latency.patch"
CACHE = ROOT / "target/android-ffmpeg"
COMPONENTS = (
    "MEDIACODEC",
    "H264_MEDIACODEC_DECODER",
    "AV1_MEDIACODEC_DECODER",
    "VP9_MEDIACODEC_DECODER",
    "H264_DEMUXER",
    "IVF_DEMUXER",
    "H264_PARSER",
    "FILE_PROTOCOL",
    "H264_MP4TOANNEXB_BSF",
    "EXTRACT_EXTRADATA_BSF",
)
LIBRARIES = ("avutil", "avcodec", "avformat")
ALLOWED_DEPENDENCIES = {
    "libavutil.so",
    "libavcodec.so",
    "libavformat.so",
    "libc.so",
    "libm.so",
    "libdl.so",
    "liblog.so",
    "libandroid.so",
    "libmediandk.so",
}


def configuration() -> tuple[Path, Path, Path]:
    selected = os.environ.get("ANDROID_NDK_HOME") or os.environ.get("ANDROID_NDK_ROOT")
    if not selected:
        raise typer.BadParameter("Android NDK environment missing")
    ndk = Path(selected)
    toolchain = ndk / "toolchains/llvm/prebuilt/linux-x86_64"
    if not (ndk / "source.properties").is_file() or not os.access(
        toolchain / "bin/aarch64-linux-android28-clang", os.X_OK
    ):
        raise typer.BadParameter("Linux x86_64 Android NDK toolchain missing")
    digest = hashlib.sha256()
    for value in (
        Path(__file__).read_bytes(),
        PATCH.read_bytes(),
        (ndk / "source.properties").read_bytes(),
        str(ndk).encode(),
        CHECKSUM.encode(),
    ):
        digest.update(len(value).to_bytes(8, "little"))
        digest.update(value)
    return ndk, toolchain, CACHE / digest.hexdigest()[:24]


def verify_archive(path: Path):
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != CHECKSUM:
        raise RuntimeError(f"FFmpeg archive checksum mismatch: {path}")


def logged(arguments, log: Path, *, cwd: Path, env=None, timeout=120):
    with log.open("w") as output:
        try:
            command = [str(arg) for arg in arguments]
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                code = process.wait(timeout=timeout)
                if code:
                    raise subprocess.CalledProcessError(code, command)
            except BaseException:
                # A timed-out make can leave compiler children after its own exit.
                # The new session belongs entirely to this build step.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
                raise
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
            typer.echo("\n".join(log.read_text().splitlines()[-60:]), err=True)
            raise


def build() -> Path:
    ndk, toolchain, prefix = configuration()
    CACHE.mkdir(parents=True, exist_ok=True)
    with (CACHE / "build.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if (prefix / "complete").is_file():
            print(f"Cached Android FFmpeg: {prefix}")
            return prefix
        if shutil.disk_usage(CACHE).free < 4 * 1024**3:
            raise RuntimeError("Need at least 4 GiB free for the build")
        archive = CACHE / f"{REVISION}.tar.gz"
        if not archive.exists():
            with tempfile.NamedTemporaryFile(dir=CACHE, prefix="download-") as download:
                subprocess.run(
                    [
                        "curl",
                        "--fail",
                        "--location",
                        "--max-time",
                        "120",
                        "--max-filesize",
                        "25000000",
                        f"https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/{REVISION}",
                        "--output",
                        download.name,
                    ],
                    check=True,
                    timeout=130,
                )
                verify_archive(Path(download.name))
                shutil.copyfile(download.name, archive)
        verify_archive(archive)
        stage = Path(tempfile.mkdtemp(prefix="stage-", dir=CACHE))
        print(f"Building FFmpeg 8.1.2, ARM64/API28, two jobs. Logs: {stage}")
        with tarfile.open(archive) as source:
            source.extractall(stage, filter="data")
        source = stage / f"FFmpeg-{REVISION}"
        with PATCH.open("rb") as patch:
            subprocess.run(
                ["patch", "--batch", "--forward", "--fuzz=0", "-p1", "-d", str(source)],
                stdin=patch,
                check=True,
            )
        work = stage / "build"
        work.mkdir()
        install = stage / "install"
        env = dict(
            os.environ,
            PATH=f"{toolchain}/bin:{os.environ['PATH']}",
            PKG_CONFIG_LIBDIR="",
            PKG_CONFIG_PATH="",
        )
        configure = [
            str(source / "configure"),
            f"--prefix={install}",
            "--target-os=android",
            "--arch=aarch64",
            "--enable-cross-compile",
        ]
        configure += [
            f"--{flag}={toolchain}/bin/{binary}"
            for flag, binary in {
                "cc": "aarch64-linux-android28-clang",
                "cxx": "aarch64-linux-android28-clang++",
                "ar": "llvm-ar",
                "ranlib": "llvm-ranlib",
                "strip": "llvm-strip",
                "nm": "llvm-nm",
            }.items()
        ]
        configure += [
            f"--sysroot={toolchain}/sysroot",
            "--disable-autodetect",
            "--disable-everything",
            "--disable-programs",
            "--disable-doc",
            "--disable-debug",
            "--disable-static",
            "--enable-shared",
            "--disable-avdevice",
            "--disable-avfilter",
            "--disable-swscale",
            "--disable-swresample",
            "--enable-avcodec",
            "--enable-avformat",
            "--enable-avutil",
            "--enable-jni",
            "--enable-mediacodec",
            "--enable-pthreads",
            "--enable-decoder=h264_mediacodec,av1_mediacodec,vp9_mediacodec",
            "--enable-parser=h264,av1,vp9",
            "--enable-demuxer=h264,ivf",
            "--enable-protocol=file",
            "--enable-bsf=h264_mp4toannexb,extract_extradata",
        ]
        logged(configure, stage / "configure.log", cwd=work, env=env)
        definitions = (work / "config.h").read_text() + (
            work / "config_components.h"
        ).read_text()
        for component in COMPONENTS:
            if not re.search(rf"^#define CONFIG_{component} 1$", definitions, re.M):
                raise RuntimeError(f"FFmpeg silently omitted {component}")
        for license in ("GPL", "NONFREE", "VERSION3"):
            if not re.search(rf"^#define CONFIG_{license} 0$", definitions, re.M):
                raise RuntimeError(f"Unexpected {license} build")
        logged(["make", "-j2"], stage / "build.log", cwd=work, env=env, timeout=900)
        logged(["make", "install"], stage / "install.log", cwd=work, env=env)
        strings = subprocess.check_output(
            [str(toolchain / "bin/llvm-strings"), str(install / "lib/libavcodec.so")],
            text=True,
        )
        if "Weld MediaCodec: requested low-latency=1" not in strings:
            raise RuntimeError("FFmpeg MediaCodec patch is missing")
        for library in LIBRARIES:
            elf = subprocess.check_output(
                [
                    str(toolchain / "bin/llvm-readelf"),
                    "-h",
                    "-d",
                    str(install / f"lib/lib{library}.so"),
                ],
                text=True,
            )
            (stage / f"{library}.elf.txt").write_text(elf)
            if not re.search(r"Machine:.*AArch64", elf) or not re.search(
                rf"\(SONAME\).*\[lib{library}\.so\]", elf
            ):
                raise RuntimeError(f"Invalid Android library: {library}")
            unexpected = (
                set(re.findall(r"\(NEEDED\).*\[(.*?)\]", elf)) - ALLOWED_DEPENDENCIES
            )
            if unexpected:
                raise RuntimeError(
                    f"Unexpected Android dependencies: {sorted(unexpected)}"
                )
        for path in [
            work / "config.h",
            work / "config_components.h",
            source / "COPYING.LGPLv2.1",
            PATCH,
        ]:
            shutil.copy2(path, install)
        (install / "complete").write_text(
            f"FFmpeg=8.1.2\nrevision={REVISION}\nsha256={CHECKSUM}\nABI=arm64-v8a\nAPI=28\nNDK={ndk}\n"
        )
        install.rename(prefix)
        print(f"Android FFmpeg ready: {prefix}\nIntermediate files retained in {stage}")
        return prefix


def main(
    print_prefix: Annotated[
        bool,
        typer.Option("--print-prefix", help="Print the cache path without building."),
    ] = False,
):
    if print_prefix:
        print(configuration()[2])
    else:
        build()


app = command(main, help=__doc__)

if __name__ == "__main__":
    app()
