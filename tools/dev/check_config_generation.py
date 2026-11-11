"""Verify checked-in configuration generation and runtime-independent contracts."""

import subprocess
import sys

from dev.cli import command
from dev.paths import ROOT


def main():
    for arguments in [
        ["-m", "dev.config_codegen.import_i3", "--check"],
        ["-m", "dev.config_codegen.generate", "--check"],
        ["-m", "unittest", "dev.tests.test_generate"],
    ]:
        subprocess.run([sys.executable, *arguments], cwd=ROOT, check=True)
    for package in ["weld-sway-config", "weldwm-config"]:
        graph = subprocess.check_output(
            [
                "cargo",
                "tree",
                "--locked",
                "--offline",
                "-p",
                package,
                "--edges",
                "normal",
                "--prefix",
                "none",
                "--format",
                "{p}",
            ],
            cwd=ROOT,
            text=True,
        )
        forbidden = [
            line
            for line in graph.splitlines()
            if line.startswith(
                (
                    "bevy",
                    "wgpu",
                    "smithay ",
                    "weld-app ",
                    "weld-core ",
                    "weld-input ",
                    "weld-tile ",
                    "weld-window ",
                    "weld-ssd ",
                    "weld-i3-quirks ",
                )
            )
        ]
        if forbidden:
            raise RuntimeError(f"{package} depends on runtime packages: {forbidden}")
        if package == "weldwm-config" and any(
            line.startswith(("weld-sway-config ", "pango "))
            for line in graph.splitlines()
        ):
            raise RuntimeError("native configuration depends on Sway syntax or Pango")
        print(f"PASS {package}: no compositor or rendering dependencies")
    host = subprocess.check_output(
        [
            "cargo",
            "tree",
            "--locked",
            "--offline",
            "-p",
            "weld-core",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ],
        cwd=ROOT,
        text=True,
    )
    if any(line.startswith("bevy") for line in host.splitlines()):
        raise RuntimeError("native host configuration dependencies enable Bevy")
    print("PASS native host: configuration contracts do not enable Bevy")
    print("PASS configuration generation and ownership boundaries")


app = command(main, help=__doc__)


if __name__ == "__main__":
    app()
