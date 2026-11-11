"""Check desktop dependency-feature parity without compiling any targets."""

import json
import subprocess
import tomllib

from dev.cli import command
from dev.paths import ROOT


def graph(target, packages, edges):
    result = subprocess.run(
        [
            "cargo",
            "tree",
            "--locked",
            "--offline",
            "--target",
            target,
            "--prefix",
            "none",
            "--format",
            "{p}|{f}",
            "--edges",
            edges,
            *packages,
        ],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    packages = {}
    for line in result.stdout.splitlines():
        if not line:
            continue
        package, separator, features = line.removesuffix(" (*)").partition("|")
        if not separator:
            raise RuntimeError(f"unexpected cargo tree record: {line}")
        packages.setdefault(package, set()).add(
            frozenset(filter(None, features.split(",")))
        )
    return packages


def main():
    metadata = json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--locked",
                "--offline",
                "--no-deps",
                "--format-version",
                "1",
            ],
            cwd=ROOT,
            text=True,
        )
    )
    application = next(
        package for package in metadata["packages"] if package["name"] == "weldwm"
    )
    if metadata["workspace_default_members"] != [application["id"]]:
        raise RuntimeError("plain Cargo commands must select only weldwm")
    if application["default_run"] != "weldwm":
        raise RuntimeError("cargo run must select the weldwm binary")
    print("PASS default Cargo package and binary: weldwm")
    with (ROOT / ".cargo/config.toml").open("rb") as source:
        aliases = tomllib.load(source)["alias"]
    compiler = subprocess.check_output(["rustc", "-vV"], text=True)
    target = next(
        line.removeprefix("host: ")
        for line in compiler.splitlines()
        if line.startswith("host: ")
    )
    baseline = graph(target, ["-p", "weldwm"], "normal,build")
    if any(package.startswith("bevy_dylib ") for package in baseline):
        raise RuntimeError("desktop graph includes bevy_dylib")
    scenarios = {
        "default build": [],
        "application tests": ["-p", "weldwm"],
        "core tests/examples": ["-p", "weldwm", "-p", "weld-core"],
        "application bridge tests": ["-p", "weldwm", "-p", "weld-app"],
        "desktop workspace": aliases["test-desktop"][1:],
    }
    for alias in ("check-desktop", "clippy-desktop"):
        scenarios[alias] = [arg for arg in aliases[alias][1:] if arg != "--all-targets"]
    for name, packages in scenarios.items():
        tested = graph(target, packages, "normal,build,dev")
        differences = [
            package
            for package in baseline.keys() & tested.keys()
            if baseline[package] != tested[package]
        ]
        if differences:
            details = "\n".join(
                f"  {package}\n    run: {baseline[package]}\n    test: {tested[package]}"
                for package in sorted(differences)
            )
            raise RuntimeError(f"{name} changes shared dependency features:\n{details}")
        print(
            f"PASS {name}: {len(baseline.keys() & tested.keys())} shared packages match"
        )
    print(f"PASS static desktop graph for {target}; no compilation performed")


app = command(main, help=__doc__)


if __name__ == "__main__":
    app()
