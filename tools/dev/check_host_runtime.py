"""Bounded real-Wayland regression; requires the normal Weld development shell.

Python 3 provides explicit process-session ownership and bounded subprocess waits.
Unlike shell setsid wrappers, start_new_session guarantees PID == process group ID.
"""

import os
import resource
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


def main(
    shm_only: Annotated[
        bool, typer.Option("--shm-only", help="force Vulkan unavailable")
    ] = False,
    nested: Annotated[
        bool,
        typer.Option("--nested", help="check native nested screenshot capture instead"),
    ] = False,
    policy_only: Annotated[
        bool,
        typer.Option(
            "--policy-only",
            help="exercise application policy without a local presenter",
        ),
    ] = False,
    frame_timings: Annotated[
        bool,
        typer.Option(
            "--frame-timings",
            help="record callback and buffer-release delay per SHM probe frame",
        ),
    ] = False,
    stalled_presenter: Annotated[
        bool,
        typer.Option(
            "--stalled-presenter",
            help="test independent pacing while native presentation never completes (no GPU)",
        ),
    ] = False,
    dormant: Annotated[
        bool,
        typer.Option(
            "--dormant", help="test the production headless host without any viewer"
        ),
    ] = False,
    reclaim_presenter: Annotated[
        bool,
        typer.Option(
            "--reclaim-presenter",
            help="test reclaim resumes native composition without another client commit (no GPU)",
        ),
    ] = False,
):
    if sum((nested, policy_only, stalled_presenter, dormant, reclaim_presenter)) > 1:
        raise typer.BadParameter("choose only one host mode")
    if nested and shm_only:
        raise typer.BadParameter(
            "--nested requires GPU rendering; cannot combine with --shm-only"
        )
    if nested and policy_only:
        raise typer.BadParameter("--policy-only cannot use a native presenter")
    if nested and frame_timings:
        raise typer.BadParameter(
            "--frame-timings requires the protocol probe, not nested screenshot capture"
        )
    if nested and (
        not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("DISPLAY"))
        or not shutil.which("foot")
    ):
        print("SKIP nested capture: requires a parent display and foot")
        return
    root = ROOT
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    build = [
        "cargo",
        "build",
        "--locked",
        "-p",
        "weldwm",
        "-p",
        "weld-core",
        "--bin",
        "weldwm",
        "--example",
        "wayland_session_probe",
        "-j8",
    ]
    if not nested and not dormant:
        build += ["--example", "presentation_host_probe"]
    if policy_only:
        build += ["--example", "policy_host_probe"]
    subprocess.run(build, cwd=root, check=True)
    validation = root / "target/validation"
    validation.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="host-runtime-", dir=validation))
    # Unix sockets are limited to 107 pathname bytes on Linux. Keep the runtime
    # short even when the repository and persistent validation logs are deep.
    runtime_owner = tempfile.TemporaryDirectory(prefix="weld-runtime-")
    runtime = Path(runtime_owner.name)
    environment = os.environ.copy()
    if nested:
        nested_capture(root, run, environment)
        return
    for key in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"):
        environment.pop(key, None)
    environment["XDG_RUNTIME_DIR"] = str(runtime)
    if shm_only:
        environment["VK_DRIVER_FILES"] = str(run / "absent-vulkan-driver.json")
        environment["VK_ICD_FILENAMES"] = environment["VK_DRIVER_FILES"]
    print(f"Logs: {run}", flush=True)
    with (run / "host.log").open("w") as log:
        host_command = [
            "cargo",
            "run",
            "--quiet",
            "--",
            "--backend",
            "headless",
            "--wayland-socket",
            "weld-probe",
            "--headless-output",
            "640x480",
            "--scale",
            "1.5",
        ]
        if policy_only:
            host_command = [
                str(root / "target/debug/examples/policy_host_probe"),
                "--socket",
                "weld-probe",
            ]
        elif not dormant:
            host_command = [
                str(root / "target/debug/examples/presentation_host_probe"),
                "--socket",
                "weld-probe",
                *(["--stalled-presenter"] if stalled_presenter else []),
                *(["--reclaim-presenter"] if reclaim_presenter else []),
            ]
        host = subprocess.Popen(
            host_command,
            cwd=root,
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 30
            socket = runtime / "weld-probe"
            while not socket.exists() or (
                policy_only
                and "POLICY_IDLE_TICKS_READY" not in (run / "host.log").read_text()
            ):
                if host.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("host did not become ready; see host.log")
                time.sleep(0.02)
            with (run / "client.log").open("w") as client_log:
                try:
                    subprocess.run(
                        [
                            str(root / "target/debug/examples/wayland_session_probe"),
                            "--expected-output",
                            "640x480",
                            "--expected-scale",
                            "2",
                            "--expected-size",
                            "1100x300",
                            *(["--dormant"] if dormant else []),
                            *(["--frame-timings"] if frame_timings else []),
                        ],
                        cwd=root,
                        env={**environment, "WAYLAND_DISPLAY": "weld-probe"},
                        stdout=client_log,
                        stderr=subprocess.STDOUT,
                        timeout=10,
                        check=True,
                    )
                except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
                    print((run / "client.log").read_text(), end="", flush=True)
                    raise
            if host.poll() is not None:
                raise RuntimeError("host exited when its last client disconnected")
            if (
                stalled_presenter
                and "STALLED_NATIVE_CALLBACKS" not in (run / "host.log").read_text()
            ):
                raise RuntimeError(
                    "stalled fixture did not stage a callback before ownership transfer"
                )
            if (
                reclaim_presenter
                and "RECLAIM_NATIVE_COMPOSITION" not in (run / "host.log").read_text()
            ):
                raise RuntimeError("reclaim did not request native composition")
            os.killpg(host.pid, signal.SIGTERM)
            if host.wait(timeout=5) != 0:
                raise RuntimeError("host did not shut down cleanly")
            if socket.exists():
                raise RuntimeError("host left its Wayland socket behind")
            print((run / "client.log").read_text(), end="")
            print("PASS client exit, host lifetime, SIGTERM shutdown, socket cleanup")
        finally:
            if host.poll() is None:
                os.killpg(host.pid, signal.SIGKILL)
                host.wait(timeout=5)


def nested_capture(root, run, environment):
    socket = run / "nested"
    screenshot = run / "capture.png"
    print(f"Logs: {run}", flush=True)
    with (run / "host.log").open("w") as log:
        host = subprocess.Popen(
            [
                "cargo",
                "run",
                "--quiet",
                "--",
                "--backend",
                "nested",
                "--config",
                str(root / "examples/master.sway.config"),
                "--wayland-socket",
                str(socket),
                "--screenshot",
                str(screenshot),
                "--",
                "foot",
            ],
            cwd=root,
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            if host.wait(timeout=30) != 0:
                raise RuntimeError("nested capture failed; see host.log")
            contents = screenshot.read_bytes()
            if not contents.startswith(b"\x89PNG\r\n\x1a\n") or len(contents) < 100:
                raise RuntimeError("capture did not produce a nonempty PNG")
            if socket.exists():
                raise RuntimeError("nested host left its socket behind")
            print("PASS nested client composition, screenshot and socket cleanup")
        finally:
            # Even success can leave a child after the compositor exits.
            try:
                os.killpg(host.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            host.wait(timeout=5)


app = command(main, help=__doc__)


if __name__ == "__main__":
    app()
