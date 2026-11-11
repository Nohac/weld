#!/usr/bin/env python3
"""Run real GPU-backed clients against paced headless presenters; save bounded logs."""

import csv
import json
import os
import resource
import shlex
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Annotated

import typer

from dev.cli import command
from dev.paths import ROOT

LAYERS = (
    "surface",
    "empty-app",
    "core-app",
    "framework-main",
    "default-main",
    "renderer-main",
    "state-direct",
    "ui-direct",
    "extract-direct",
    "prepare-direct",
    "render-direct",
    "minimal",
    "master",
    "minimal-retained",
    "master-retained",
)


def collect_counters(process, log_path, directory, perf, seconds):
    deadline = time.monotonic() + 40
    while "MEASUREMENT_START" not in log_path.read_text():
        if process.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError("benchmark did not begin its measurement window")
        time.sleep(0.02)
    started = time.monotonic()
    with (directory / "counters.csv").open("w") as counters:
        subprocess.run(
            [
                str(perf),
                "stat",
                "-x",
                ";",
                "-e",
                "instructions:u,cycles:u",
                "-p",
                str(process.pid),
                "--",
                "sleep",
                str(seconds - 2),
            ],
            stderr=counters,
            check=True,
            timeout=seconds + 5,
            env={**os.environ, "LC_ALL": "C"},
        )
    duration = time.monotonic() - started
    if process.poll() is not None:
        raise RuntimeError("benchmark ended during counter collection")
    values = {}
    with (directory / "counters.csv").open() as counters:
        for row in csv.reader(counters, delimiter=";"):
            if len(row) >= 5 and row[2] in ("instructions:u", "cycles:u"):
                try:
                    count, coverage = int(row[0]), float(row[4])
                except ValueError as error:
                    raise RuntimeError(f"perf did not count {row[2]}") from error
                if count <= 0 or coverage < 99:
                    raise RuntimeError(f"insufficient perf coverage for {row[2]}")
                values[row[2]] = count
    if len(values) != 2:
        raise RuntimeError("perf did not return both requested counters")
    (directory / "counter-window.json").write_text(
        json.dumps(
            {
                "seconds": duration,
                "counts": values,
                "scope": "userspace, attached Weld process and threads; interior measurement window",
            },
            indent=2,
        )
    )


def main(
    mode: Annotated[str, typer.Option("--mode")] = "all",
    repeat: Annotated[int, typer.Option("--repeat")] = 1,
    perf: Annotated[Path | None, typer.Option("--perf")] = None,
    seconds: Annotated[int, typer.Option("--seconds")] = 10,
    warmup: Annotated[int, typer.Option("--warmup")] = 3,
    width: Annotated[int, typer.Option("--width")] = 1920,
    height: Annotated[int, typer.Option("--height")] = 1080,
    hz: Annotated[int, typer.Option("--hz")] = 60,
    producer_hz: Annotated[int, typer.Option("--producer-hz")] = 120,
    input_hz: Annotated[int, typer.Option("--input-hz")] = 0,
    focus_hz: Annotated[int, typer.Option("--focus-hz")] = 0,
    tabbed: Annotated[bool, typer.Option("--tabbed")] = False,
    windows: Annotated[int, typer.Option("--windows")] = 1,
    decorations: Annotated[str, typer.Option("--decorations")] = "client",
    jobs: Annotated[int, typer.Option("--jobs")] = 8,
    no_build: Annotated[bool, typer.Option("--no-build")] = False,
    executable: Annotated[Path | None, typer.Option("--executable")] = None,
    scanner: Annotated[Path | None, typer.Option("--scanner")] = None,
):
    options = dict(
        mode=mode,
        repeat=repeat,
        perf=perf,
        seconds=seconds,
        warmup=warmup,
        width=width,
        height=height,
        hz=hz,
        producer_hz=producer_hz,
        input_hz=input_hz,
        focus_hz=focus_hz,
        tabbed=tabbed,
        windows=windows,
        decorations=decorations,
        jobs=jobs,
        no_build=no_build,
        executable=executable,
        scanner=scanner,
    )
    if mode not in (*LAYERS, "hidden", "all") or decorations not in (
        "client",
        "server",
    ):
        raise typer.BadParameter("invalid benchmark mode or decoration policy")
    if not 0 <= focus_hz <= 30:
        raise typer.BadParameter("focus rate must be 0..30 Hz")
    if (focus_hz or tabbed) and (
        mode not in ("master", "master-retained") or windows < 2
    ):
        raise typer.BadParameter(
            "focus/tabbed scenarios require Master mode and at least two windows"
        )
    if executable and not no_build:
        raise typer.BadParameter("--executable requires --no-build")
    if executable:
        try:
            executable = executable.resolve(strict=True)
        except OSError as error:
            raise typer.BadParameter(f"cannot use benchmark binary: {error}")
    if not 1 <= repeat <= 10:
        raise typer.BadParameter("repeat must be 1..10")
    if perf and seconds < 4:
        raise typer.BadParameter(
            "counter collection requires at least four measurement seconds"
        )
    if perf:
        try:
            perf = perf.resolve(strict=True)
        except OSError as error:
            raise typer.BadParameter(f"cannot use perf binary: {error}")
    if not (
        1 <= seconds <= 120
        and 0 <= warmup <= 30
        and 1 <= windows <= 8
        and 1 <= hz <= 240
        and 1 <= producer_hz <= 1000
        and 0 <= input_hz <= 2000
        and 64 <= width <= 8192
        and 64 <= height <= 8192
    ):
        raise typer.BadParameter(
            "invalid workload bounds; use --help for the available controls"
        )
    root = ROOT
    os.chdir(root)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    build = root / "target/validation/paced-render-build"
    build.mkdir(parents=True, exist_ok=True)
    producer = build / "egl-producer"
    manifest = build / "executable.json"
    if not no_build:
        scanner_binary = str(scanner) if scanner else shutil.which("wayland-scanner")
        if not scanner_binary:
            raise typer.BadParameter(
                "wayland-scanner is required; pass --scanner /path/to/wayland-scanner"
            )
        protocol_dir = subprocess.check_output(
            ["pkg-config", "--variable=pkgdatadir", "wayland-protocols"], text=True
        ).strip()
        protocol = Path(protocol_dir) / "stable/xdg-shell/xdg-shell.xml"
        subprocess.run(
            [
                scanner_binary,
                "client-header",
                str(protocol),
                str(build / "xdg-shell-client-protocol.h"),
            ],
            check=True,
        )
        subprocess.run(
            [
                scanner_binary,
                "private-code",
                str(protocol),
                str(build / "xdg-shell-protocol.c"),
            ],
            check=True,
        )
        decoration_protocol = (
            Path(protocol_dir)
            / "unstable/xdg-decoration/xdg-decoration-unstable-v1.xml"
        )
        subprocess.run(
            [
                scanner_binary,
                "client-header",
                str(decoration_protocol),
                str(build / "xdg-decoration-client-protocol.h"),
            ],
            check=True,
        )
        subprocess.run(
            [
                scanner_binary,
                "private-code",
                str(decoration_protocol),
                str(build / "xdg-decoration-protocol.c"),
            ],
            check=True,
        )
        flags = shlex.split(
            subprocess.check_output(
                [
                    "pkg-config",
                    "--cflags",
                    "--libs",
                    "wayland-client",
                    "wayland-egl",
                    "egl",
                    "glesv2",
                ],
                text=True,
            )
        )
        subprocess.run(
            [
                "cc",
                "-O3",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-I",
                str(build),
                "scripts/profiling/egl-producer.c",
                str(build / "xdg-shell-protocol.c"),
                str(build / "xdg-decoration-protocol.c"),
                "-o",
                str(producer),
                *flags,
            ],
            check=True,
        )
        built_executable: str | None = None
        with subprocess.Popen(
            [
                "cargo",
                "bench",
                "--no-run",
                "--profile",
                "release",
                "--bench",
                "paced_render",
                "--locked",
                "-j",
                str(jobs),
                "--message-format=json",
            ],
            stdout=subprocess.PIPE,
            text=True,
        ) as cargo:
            if cargo.stdout is None:
                raise RuntimeError("Cargo output pipe was not created")
            for line in cargo.stdout:
                event = json.loads(line)
                if (
                    event.get("reason") == "compiler-artifact"
                    and event.get("target", {}).get("name") == "paced_render"
                ):
                    built_executable = event.get("executable") or built_executable
                if event.get("reason") == "compiler-message":
                    print(event["message"].get("rendered", ""), end="")
            if cargo.wait() != 0 or not built_executable:
                raise RuntimeError("benchmark build failed")
        manifest.write_text(json.dumps({"executable": built_executable}))
    benchmark_executable = (
        str(executable)
        if executable
        else json.loads(manifest.read_text())["executable"]
    )
    resource.setrlimit(resource.RLIMIT_FSIZE, (128 * 1024 * 1024, 128 * 1024 * 1024))
    run = Path(tempfile.mkdtemp(prefix="paced-render-", dir=root / "target/validation"))
    print(f"Results: {run}", flush=True)
    environment = {
        **os.environ,
        "VK_LOADER_LAYERS_DISABLE": "VK_LAYER_KHRONOS_validation",
        "RUST_LOG": "warn",
    }
    (run / "settings.json").write_text(
        json.dumps(
            {
                "options": {
                    key: str(value) if isinstance(value, Path) else value
                    for key, value in options.items()
                },
                "executable": benchmark_executable,
                "git_head": subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], text=True
                ).strip(),
                "kernel": os.uname().release,
                "started_unix": time.time(),
                "cpu_accounting": "Weld process user+system; excludes producer and post-run GPU capture",
            },
            indent=2,
        )
    )
    modes = LAYERS if mode == "all" else (mode,)
    cases = [
        (iteration, mode)
        for iteration in range(repeat)
        for mode in (modes if iteration % 2 == 0 else tuple(reversed(modes)))
    ]
    for iteration, mode in cases:
        directory = run / (mode if repeat == 1 else f"{iteration + 1}-{mode}")
        directory.mkdir()
        command = [
            benchmark_executable,
            "--mode",
            mode,
            "--producer",
            str(producer),
            "--run-dir",
            str(directory),
            "--decorations",
            decorations,
        ]
        if focus_hz:
            command += ["--focus-hz", str(focus_hz)]
        if tabbed:
            command += ["--tabbed"]
        for name in (
            "seconds",
            "warmup",
            "width",
            "height",
            "hz",
            "producer_hz",
            "input_hz",
            "windows",
        ):
            command += ["--" + name.replace("_", "-"), str(options[name])]
        with (directory / "run.log").open("w") as log:
            process = subprocess.Popen(
                command,
                env=environment,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                if perf:
                    collect_counters(
                        process, directory / "run.log", directory, perf, seconds
                    )
                code = process.wait(timeout=seconds + warmup + 30)
            finally:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
        lines = (directory / "run.log").read_text().splitlines()
        for line in lines:
            if line.startswith(
                ("CASE", "ADAPTER", "RESULT", "RETAINED_VALIDATION", "POLICY")
            ):
                print(line, flush=True)
        if code != 0 or not any(line.startswith("RESULT") for line in lines):
            raise RuntimeError(f"{mode} failed: inspect {directory / 'run.log'}")
        if focus_hz:
            focus_changes = [
                line for line in lines if line.startswith("FOCUS window=Some")
            ]
            if tabbed:
                focus_changes = [
                    line for line in focus_changes if "tabbed=true" in line
                ]
            if len(focus_changes) < 3:
                raise RuntimeError(
                    "focus scenario did not switch between managed windows"
                )
        if mode.endswith("-retained"):
            checks = [
                index
                for index, line in enumerate(lines)
                if line.startswith("RETAINED_VALIDATION")
            ]
            measurement = next(
                (
                    index
                    for index, line in enumerate(lines)
                    if line.startswith("MEASUREMENT_START")
                ),
                -1,
            )
            if len(checks) != 3 or any(index >= measurement for index in checks):
                raise RuntimeError(
                    "retained validation must finish during warmup; increase --warmup"
                )


app = command(main, help=__doc__)

if __name__ == "__main__":
    app()
