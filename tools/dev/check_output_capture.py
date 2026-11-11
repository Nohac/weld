"""Bounded DMA-BUF/SHM capture validation in a private nested Weld session."""

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Annotated

import typer

from dev.cli import command
from dev.paths import ROOT, portal_python


def main(
    visible: Annotated[
        bool, typer.Option("--visible", help="explicitly run on the current desktop")
    ] = False,
    portal: Annotated[
        bool,
        typer.Option(
            "--portal",
            help="also check a private portal session (requires PyGObject and WELD_TEST_PORTAL_FRONTEND/BACKEND)",
        ),
    ] = False,
    kooha: Annotated[
        bool,
        typer.Option(
            "--kooha",
            help="also record a short video with installed Kooha in the private portal session",
        ),
    ] = False,
    kooha_region: Annotated[
        bool,
        typer.Option(
            "--kooha-region",
            help="exercise Kooha's region selection and cropped recording",
        ),
    ] = False,
    kooha_window: Annotated[
        bool,
        typer.Option(
            "--kooha-window",
            help="record a window on an inactive workspace through the portal",
        ),
    ] = False,
    xwayland: Annotated[
        bool,
        typer.Option(
            "--xwayland",
            help="also validate a static X11 window in the private XWayland server",
        ),
    ] = False,
):
    root = ROOT
    subprocess.run(
        ["cargo", "build", "--locked", "-p", "weldwm", "--bins"], cwd=root, check=True
    )
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "-p",
            "weldwm",
            "-p",
            "weld-core",
            "--example",
            "output_capture_probe",
        ],
        cwd=root,
        check=True,
    )
    if xwayland:
        subprocess.run(
            [
                "cargo",
                "build",
                "--locked",
                "-p",
                "weldwm",
                "-p",
                "weld-core",
                "--example",
                "x11_window_probe",
            ],
            cwd=root,
            check=True,
        )
    run = Path(
        tempfile.mkdtemp(prefix="output-capture-", dir=root / "target/validation")
    )
    config = run / "config"
    config.write_text("font monospace 14\ndefault_border pixel 2\n")
    socket = run / "wayland"
    env = dict(
        os.environ,
        RUST_LOG="warn,weld_capture=trace,weld_render_state=debug",
        WGPU_VALIDATION="1",
    )
    env.pop("WAYLAND_SOCKET", None)
    env["PIPEWIRE_REMOTE"] = os.environ.get(
        "PIPEWIRE_REMOTE", str(Path(os.environ["XDG_RUNTIME_DIR"]) / "pipewire-0")
    )
    children = []
    private_runtime = None
    try:
        if not visible:
            weston = os.environ.get("WELD_TEST_WESTON") or shutil.which("weston")
            desktop = ":".join(
                [
                    os.environ.get("XDG_CURRENT_DESKTOP", ""),
                    os.environ.get("XDG_SESSION_DESKTOP", ""),
                ]
            ).lower()
            headless_error = None
            if weston:
                private_runtime = tempfile.TemporaryDirectory(prefix="weld-capture-")
                runtime = Path(private_runtime.name)
                parent_env = dict(env, XDG_RUNTIME_DIR=str(runtime))
                # Packaged Weston carries its own runtime dependencies.
                parent_env.pop("LD_LIBRARY_PATH", None)
                with (run / "weston.log").open("w") as parent_log:
                    parent = subprocess.Popen(
                        [
                            weston,
                            "--backend=headless",
                            "--renderer=gl",
                            "--socket=capture-parent",
                            "--idle-time=0",
                            "--no-config",
                        ],
                        env=parent_env,
                        stdout=parent_log,
                        stderr=subprocess.STDOUT,
                        start_new_session=True,
                    )
                    children.append(parent)
                    deadline = time.monotonic() + 10
                    while not (runtime / "capture-parent").exists():
                        if parent.poll() is not None or time.monotonic() >= deadline:
                            headless_error = (
                                f"headless Weston failed; see {run / 'weston.log'}"
                            )
                            break
                        time.sleep(0.05)
                    if headless_error is None:
                        env.update(
                            XDG_RUNTIME_DIR=str(runtime),
                            WAYLAND_DISPLAY="capture-parent",
                        )
                        print("Using a private headless Weston parent", flush=True)
            else:
                headless_error = (
                    "Weston is unavailable; set WELD_TEST_WESTON or add weston to PATH"
                )
            if headless_error:
                if (
                    "weld" in desktop
                    or not desktop.strip(":")
                    or not (env.get("WAYLAND_DISPLAY") or env.get("DISPLAY"))
                ):
                    raise RuntimeError(
                        headless_error
                        + "; use --visible only for an intentional desktop test"
                    )
                print(
                    headless_error
                    + "; falling back to a visible nested run on "
                    + desktop,
                    flush=True,
                )
        client_env = dict(env, WAYLAND_DISPLAY=str(socket))
        with (run / "host.log").open("w") as log:
            host = subprocess.Popen(
                [
                    str(root / "target/debug/weldwm"),
                    "--backend",
                    "nested",
                    "--wayland-socket",
                    str(socket),
                    "--config",
                    str(config),
                    *(["--xwayland"] if xwayland else []),
                ],
                cwd=root,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            children.append(host)
            deadline = time.monotonic() + 30
            while not socket.exists():
                if host.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"host failed: {run / 'host.log'}")
                time.sleep(0.05)
            with (run / "client.log").open("w") as client_log:
                client = subprocess.Popen(
                    [
                        "foot",
                        "--title=Capture validation",
                        "sh",
                        "-c",
                        "printf '\033[44m\033[2J\033[HCapture validation\n'; sleep 120",
                    ],
                    env=client_env,
                    stdout=client_log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                children.append(client)
                time.sleep(2)
                probe = str(root / "target/debug/examples/output_capture_probe")
                subprocess.run(
                    [probe, "--frames", "4", "--compare"],
                    env=client_env,
                    check=True,
                    timeout=20,
                )
                subprocess.run(
                    [
                        probe,
                        "--frames",
                        "4",
                        "--compare",
                        "--window-title",
                        "Capture validation",
                    ],
                    env=client_env,
                    check=True,
                    timeout=20,
                )
                # A separate application lets the protocol test cover a size
                # change and destruction without touching the static fixture.
                lifecycle_client = subprocess.Popen(
                    [
                        "foot",
                        "--title=Capture lifecycle validation",
                        "sh",
                        "-c",
                        "sleep 60",
                    ],
                    env=client_env,
                    stdout=client_log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                children.append(lifecycle_client)
                ctl = [
                    str(root / "target/debug/weldctl"),
                    "--session",
                    str(socket),
                    "window",
                ]

                def window_command(*arguments):
                    return json.loads(
                        subprocess.check_output(
                            [*ctl, *arguments, "--json"],
                            env=client_env,
                            text=True,
                            timeout=5,
                        )
                    )

                deadline = time.monotonic() + 10
                while True:
                    windows = window_command("list")["windows"]
                    target = next(
                        (
                            item
                            for item in windows
                            if item.get("title") == "Capture lifecycle validation"
                        ),
                        None,
                    )
                    if target:
                        break
                    if time.monotonic() >= deadline:
                        raise RuntimeError("lifecycle fixture did not map")
                    time.sleep(0.1)
                window_command("control", target["id"], "--enabled")
                while not target.get("control"):
                    if time.monotonic() >= deadline:
                        raise RuntimeError("lifecycle fixture did not acquire control")
                    target = next(
                        item
                        for item in window_command("list")["windows"]
                        if item["id"] == target["id"]
                    )
                    time.sleep(0.05)
                marker = run / "window-lifecycle"
                lifecycle = subprocess.Popen(
                    [
                        probe,
                        "--window-title",
                        "Capture lifecycle validation",
                        "--frames",
                        "4",
                        "--lifecycle",
                        str(marker),
                    ],
                    env=client_env,
                    start_new_session=True,
                )
                children.append(lifecycle)

                def wait_marker(extension):
                    deadline = time.monotonic() + 10
                    while not marker.with_suffix(extension).exists():
                        if lifecycle.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError(
                                "window lifecycle probe stalled at " + extension
                            )
                        time.sleep(0.05)

                wait_marker(".ready")
                window_command(
                    "resize",
                    "--control",
                    target["control"],
                    "--width",
                    "400",
                    "--height",
                    "300",
                )
                wait_marker(".resized")
                lifecycle_client.terminate()
                lifecycle_client.wait(timeout=5)
                if lifecycle.wait(timeout=10) != 0:
                    raise RuntimeError("window lifecycle probe failed")
                if xwayland:
                    x11_title = "Capture X11 validation"
                    subprocess.run(
                        [
                            str(root / "target/debug/weldctl"),
                            "--session",
                            str(socket),
                            "run",
                            "--shared-display",
                            "--",
                            str(root / "target/debug/examples/x11_window_probe"),
                            "--seconds",
                            "30",
                            "--static-content",
                            "--title",
                            x11_title,
                        ],
                        env=client_env,
                        check=True,
                        timeout=10,
                    )
                    deadline = time.monotonic() + 10
                    while not any(
                        item.get("title") == x11_title
                        for item in window_command("list")["windows"]
                    ):
                        if time.monotonic() >= deadline:
                            raise RuntimeError("private X11 fixture did not map")
                        time.sleep(0.1)
                    time.sleep(0.3)
                    subprocess.run(
                        [
                            probe,
                            "--window-title",
                            x11_title,
                            "--frames",
                            "4",
                            "--compare",
                            "--corner-rgb",
                            "204050",
                        ],
                        env=client_env,
                        check=True,
                        timeout=20,
                    )
                observations = []

                def cpu_seconds():
                    fields = (
                        Path(f"/proc/{host.pid}/stat")
                        .read_text()
                        .rsplit(")", 1)[1]
                        .split()
                    )
                    return (int(fields[11]) + int(fields[12])) / os.sysconf(
                        "SC_CLK_TCK"
                    )

                for fps in [30, 60]:
                    before = cpu_seconds()
                    compositions_before = (
                        (run / "host.log")
                        .read_text()
                        .count("completed output composition")
                    )
                    start = time.monotonic()
                    subprocess.run(
                        [
                            probe,
                            "--frames",
                            str(fps * 3),
                            "--fps",
                            str(fps),
                            "--cursor",
                        ],
                        env=client_env,
                        check=True,
                        timeout=20,
                    )
                    elapsed = time.monotonic() - start
                    compositions = (run / "host.log").read_text().count(
                        "completed output composition"
                    ) - compositions_before
                    assert compositions < fps, (
                        f"capture triggered {compositions} scene compositions in three seconds"
                    )
                    observations.append(
                        {
                            "fps": fps,
                            "cpu_seconds": cpu_seconds() - before,
                            "wall_seconds": elapsed,
                            "scene_compositions": compositions,
                        }
                    )
                compositions_before = (
                    (run / "host.log").read_text().count("completed output composition")
                )
                subprocess.run(
                    [
                        probe,
                        "--window-title",
                        "Capture validation",
                        "--frames",
                        "180",
                        "--fps",
                        "60",
                        "--fresh-sessions",
                    ],
                    env=client_env,
                    check=True,
                    timeout=20,
                )
                window_compositions = (run / "host.log").read_text().count(
                    "completed output composition"
                ) - compositions_before
                assert window_compositions == 0, (
                    f"window capture triggered {window_compositions} desktop compositions"
                )
                print(
                    "PASS 60 fps independent window snapshots without additional desktop composition",
                    flush=True,
                )
                idle_marker = run / "idle-window"
                idle_script = run / "idle-window.py"
                idle_script.write_text(
                    "import pathlib,time\nprint('\\033[?25l\\033[44m\\033[2J\\033[HIdle capture',flush=True)\nmarker=pathlib.Path("
                    + repr(str(idle_marker.with_suffix(".ready")))
                    + ")\nwhile not marker.exists():time.sleep(0.02)\nprint('\\033[41m\\033[2J\\033[HResumed capture',flush=True)\ntime.sleep(30)\n"
                )
                idle_client = subprocess.Popen(
                    [
                        "foot",
                        "--title=Capture idle validation",
                        "python3",
                        str(idle_script),
                    ],
                    env=client_env,
                    stdout=client_log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                children.append(idle_client)
                time.sleep(2)
                idle_before = (
                    (run / "host.log").read_text().count("submitted window capture")
                )
                subprocess.run(
                    [
                        probe,
                        "--window-title",
                        "Capture idle validation",
                        "--idle-ms",
                        "11000",
                        "--resume-marker",
                        str(idle_marker),
                    ],
                    env=client_env,
                    check=True,
                    timeout=20,
                )
                idle_copies = (run / "host.log").read_text().count(
                    "submitted window capture"
                ) - idle_before
                assert idle_copies == 2, (
                    f"idle/resume should copy only initial and changed content, got {idle_copies}"
                )
                idle_client.terminate()
                idle_client.wait(timeout=5)
                paced_code = "import time; [(print('\\033[?25l\\033[41m\\033[2J\\033[HRED' if i%2 else '\\033[?25l\\033[44m\\033[2J\\033[HBLUE',flush=True),time.sleep(1/60)) for i in range(600)]"
                subprocess.run(
                    [
                        str(root / "target/debug/weldctl"),
                        "--session",
                        str(socket),
                        "run",
                        "--workspace",
                        "capture-pacing",
                        "--owner",
                        "capture-test",
                        "--",
                        "foot",
                        "--title=Capture paced validation",
                        "python3",
                        "-c",
                        paced_code,
                    ],
                    env=client_env,
                    stdout=subprocess.DEVNULL,
                    check=True,
                    timeout=10,
                )
                deadline = time.monotonic() + 10
                while not any(
                    item.get("title") == "Capture paced validation"
                    for item in window_command("list")["windows"]
                ):
                    if time.monotonic() >= deadline:
                        raise RuntimeError("paced fixture did not map")
                    time.sleep(0.05)
                # Starting capture wakes a hidden client's pending configure.
                # Reopen with the new constraints before timing steady delivery.
                for attempt in range(3):
                    warmed = subprocess.run(
                        [
                            probe,
                            "--window-title",
                            "Capture paced validation",
                            "--frames",
                            "1",
                            "--fresh-sessions",
                        ],
                        env=client_env,
                        capture_output=True,
                        text=True,
                        timeout=10,
                    )
                    (run / f"paced-warmup-{attempt}.log").write_text(
                        warmed.stdout + warmed.stderr
                    )
                    if warmed.returncode == 0:
                        break
                    if "capture failed: Value(BufferConstraints)" not in warmed.stderr:
                        raise RuntimeError(
                            "paced fixture warmup failed: " + warmed.stderr
                        )
                else:
                    raise RuntimeError("paced fixture constraints did not settle")
                time.sleep(1)
                before = (
                    (run / "host.log").read_text().count("completed output composition")
                )
                start = time.monotonic()
                subprocess.run(
                    [
                        probe,
                        "--window-title",
                        "Capture paced validation",
                        "--frames",
                        "120",
                        "--fps",
                        "240",
                    ],
                    env=client_env,
                    check=True,
                    timeout=10,
                )
                elapsed = time.monotonic() - start
                assert elapsed >= 1.5, (
                    f"consumer outran the 60 Hz producer: {elapsed}s for 120 frames"
                )
                extra_compositions = (run / "host.log").read_text().count(
                    "completed output composition"
                ) - before
                assert extra_compositions == 0, (
                    f"hidden stream triggered {extra_compositions} desktop compositions"
                )
                observations.append(
                    {
                        "window_stream": True,
                        "requested_fps": 240,
                        "frames": 120,
                        "wall_seconds": elapsed,
                        "scene_compositions": extra_compositions,
                    }
                )
                print(
                    f"PASS eager 240 fps consumer followed hidden 60 Hz source: 120 frames in {elapsed:.3f}s",
                    flush=True,
                )
                # Disconnect mid-capture, then prove admission and imports remain usable.
                cancelled = subprocess.Popen(
                    [probe, "--frames", "1000"],
                    env=client_env,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                try:
                    time.sleep(0.05)
                finally:
                    cancelled.terminate()
                    cancelled.wait(timeout=5)
                subprocess.run(
                    [probe, "--frames", "4", "--compare"],
                    env=client_env,
                    check=True,
                    timeout=20,
                )
                (run / "measurements.json").write_text(
                    json.dumps(observations, indent=2) + "\n"
                )
                print(
                    "PASS DMA-BUF reuse, cursor capture, and capture after disconnect",
                    flush=True,
                )
                if portal or kooha or kooha_region or kooha_window:
                    portal_process = subprocess.Popen(
                        [
                            "dbus-run-session",
                            "--",
                            portal_python(),
                            str(root / "scripts/fixtures/screencast_portal.py"),
                            str(run),
                            *(
                                ["--kooha"]
                                if kooha or kooha_region or kooha_window
                                else []
                            ),
                            *(["--region"] if kooha_region else []),
                            *(["--window"] if kooha_window else []),
                        ],
                        env=client_env,
                        start_new_session=True,
                    )
                    children.append(portal_process)
                    if portal_process.wait(timeout=65) != 0:
                        raise RuntimeError("private portal capture failed")
        errors = (run / "host.log").read_text()
        if any(
            marker in errors
            for marker in [
                "Validation Error",
                "VUID-",
                "SYNC-HAZARD",
                "panicked at",
                "could not submit DMA-BUF",
            ]
        ):
            raise RuntimeError(f"GPU/host error: {run / 'host.log'}")
    finally:
        for child in reversed(children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
        if private_runtime is not None:
            private_runtime.cleanup()
        print(f"Logs and captures: {run}", flush=True)


app = command(main, help=__doc__)


if __name__ == "__main__":
    app()
