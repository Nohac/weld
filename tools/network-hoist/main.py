#!/usr/bin/env python3
"""User-side entry point, Cargo runtime capture, and private supervisor dispatch."""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import uuid

# -I excludes the working directory; load only the module next to this entry point.
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from policy import Refused, interface_name, run_id, seconds
import lifecycle


ENVIRONMENT = (
    "HOME", "PATH", "LD_LIBRARY_PATH", "XDG_RUNTIME_DIR", "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS", "XDG_DATA_DIRS", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
    "XDG_CACHE_HOME", "XDG_STATE_HOME", "LANG", "LC_ALL", "RUST_LOG", "RUST_BACKTRACE",
    "LIBVA_DRIVER_NAME", "LIBVA_DRIVERS_PATH", "LIBGL_DRIVERS_PATH", "GBM_BACKENDS_PATH",
    "VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_LAYER_PATH", "VK_ADD_LAYER_PATH",
    "__EGL_VENDOR_LIBRARY_FILENAMES", "WGPU_BACKEND", "WGPU_ADAPTER_NAME",
    "SSL_CERT_FILE", "NIX_SSL_CERT_FILE", "WELD_LEGACY_KEY_REPEAT",
    "WELD_HOIST_BITRATE_TARGET_MBPS",
)


def save_private(path, value):
    with open(path, "x", encoding="utf-8", opener=lambda name, flags: os.open(name, flags, 0o600)) as output:
        json.dump(value, output)


def capture(path, binary):
    if os.geteuid() == 0:
        raise Refused("Cargo capture must run as the normal user")
    save_private(path, {
        "binary": str(Path(binary).resolve()),
        "environment": {name: os.environ[name] for name in ENVIRONMENT if name in os.environ},
    })


def execute_user(path, role):
    config = lifecycle.read_private(Path(path), os.geteuid())
    if os.geteuid() == 0 or config["uid"] != os.geteuid() or role not in ("source", "destination"):
        raise Refused("Weld may only run as the original unprivileged user")
    directory = Path(path).parent
    environment = config["runtime"]["environment"]
    environment.update({"GDK_BACKEND": "wayland", "QT_QPA_PLATFORM": "wayland",
                        "WINIT_UNIX_BACKEND": "wayland", "MOZ_ENABLE_WAYLAND": "1",
                        "XDG_SESSION_TYPE": "wayland"})
    environment.setdefault("RUST_LOG", "warn,weldwm=info,weld_core=info,weld_network_diag=debug,weld_media_diag=debug")
    args = [config["runtime"]["binary"], "--backend", "nested", "--wayland-socket",
            f"weld-net-{config['id'][:12]}-{role}", "--hoist-iroh-network", "n0",
            "--hoist-iroh-timeout", str(config["startup_seconds"])]
    if role == "source":
        args += ["--hoist-iroh-listen", str(directory / "source.ticket"),
                 "--hoist-iroh-expect-peer", str(directory / "destination.identity"),
                 "--hoist-codec", config["codec"], "--", *config["command"]]
    else:
        args += ["--hoist-iroh-connect", str(directory / "source.ticket"),
                 "--hoist-iroh-publish-identity", str(directory / "destination.identity")]
    os.umask(0o077)
    fd = os.open(directory / f"{role}.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    os.dup2(fd, 1)
    os.dup2(fd, 2)
    os.close(fd)
    os.chdir(config["repository"])
    os.execve(args[0], args, environment)


def supervise(arguments):
    # Callers must pass a foreground sudo command so its password prompt works. It
    # forwards SIGINT to _prepare, which owns the exact-unit stop and recovery.
    # Raising KeyboardInterrupt here would make subprocess.call kill that owner.
    with lifecycle.record_interrupts() as interruption:
        if interruption.requested:
            return 130
        process = subprocess.Popen(arguments)
        if interruption.requested:
            # Cover a signal received by the parent before the child existed.
            process.send_signal(signal.SIGINT)
        result = process.wait()
        return 130 if interruption.requested and result == 0 else result


def launch():
    parser = argparse.ArgumentParser(prog="scripts/run-network-hoist", description="Bounded, intended-peer-only Weld Wi-Fi/USB network test")
    parser.add_argument("--host", type=interface_name, help="uplink left in the host namespace")
    parser.add_argument("--client", type=interface_name, help="physical Ethernet/USB interface temporarily isolated")
    parser.add_argument("--dhcpcd", default=shutil.which("dhcpcd"), help="absolute dhcpcd 10.x executable (required if not on PATH)")
    parser.add_argument("--seconds", type=seconds, default=120)
    parser.add_argument("--startup-seconds", type=seconds, default=120)
    parser.add_argument("--codec", choices=("av1", "h264"), default="av1")
    parser.add_argument("--dry-run", action="store_true", help="read-only preflight; no build, sudo, or network changes")
    parser.add_argument("--recover", type=run_id, metavar="RUN_ID", help="retry the same privileged cleanup for an interrupted run")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="source application after -- (default foot)")
    args = parser.parse_args()
    if os.geteuid() == 0:
        raise Refused("run this launcher as your normal user, not with sudo")
    if args.recover:
        helper = lifecycle.ROOT / args.recover / "main.py"
        return supervise(["sudo", str(helper), "_recover", args.recover])
    if not args.host or not args.client or not args.dhcpcd:
        parser.error("--host, --client and an available --dhcpcd executable are required")
    snapshot = lifecycle.preflight(args.host, args.client, args.dhcpcd)
    print(f"Host: {args.host}; receiver: {args.client}; codec: {args.codec}")
    print(f"Profile is persistent and interface-bound; runtime {args.seconds}s after startup.")
    print("Only the client interface will move. No profile settings, host routes, or host DNS will be edited.")
    print("Phone Wi-Fi must be OFF. This will contact N0 discovery/relay services over the Internet.")
    if args.dry_run:
        print("Preflight passed. No build, elevation, interfaces, namespaces, or services were changed.")
        return 0
    repository = Path(__file__).resolve().parents[2]
    validation = repository / "target" / "validation"
    validation.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="network-hoist-", dir=validation))
    for filename in ("main.py", "lifecycle.py", "policy.py"):
        target_path = directory / filename
        target_path.write_bytes((Path(__file__).parent / filename).read_bytes())
        target_path.chmod(0o500)
    entry = directory / "main.py"
    context = directory / "runtime.json"
    rust = subprocess.check_output(["rustc", "-vV"], text=True)
    target = next(line.removeprefix("host: ") for line in rust.splitlines() if line.startswith("host: "))
    runner = [sys.executable, "-I", str(entry), "_capture", str(context)]
    # A one-shot Cargo override, not a config-file or compilation-profile change.
    subprocess.run(["cargo", "run", "--offline", "--bin", "weldwm", "--config",
                    f"target.{target}.runner={json.dumps(runner)}"], cwd=repository, check=True)
    runtime = lifecycle.read_private(context, os.getuid())
    runtime_dir = Path(runtime["environment"].get("XDG_RUNTIME_DIR", ""))
    display = runtime["environment"].get("WAYLAND_DISPLAY", "")
    if not display or not (runtime_dir / display).is_socket():
        raise Refused("a filesystem-backed parent Wayland display is required")
    identifier = uuid.uuid4().hex
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    config = {"id": identifier, "uid": os.getuid(), "gid": os.getgid(),
              "host": args.host, "client": args.client, "dhcpcd": str(Path(args.dhcpcd).resolve()),
              "seconds": args.seconds, "startup_seconds": args.startup_seconds,
              "codec": args.codec, "command": command or ["foot"], "runtime": runtime,
              "repository": str(repository), "snapshot": snapshot}
    manifest = directory / "run.json"
    save_private(manifest, config)
    print(f"Logs: {directory}\nRecovery: scripts/run-network-hoist --recover {identifier}", flush=True)
    try:
        return supervise(["sudo", sys.executable, "-I", str(entry), "_prepare", str(manifest)])
    finally:
        for name in ("source.ticket", "destination.identity"):
            (directory / name).unlink(missing_ok=True)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "_capture":
        capture(sys.argv[2], sys.argv[3])
        return 0
    if len(sys.argv) > 1 and sys.argv[1] == "_exec":
        execute_user(sys.argv[2], sys.argv[3])
    if len(sys.argv) > 1 and sys.argv[1].startswith("_"):
        return lifecycle.dispatch(sys.argv[1], sys.argv[2:])
    if os.environ.get("WELD_NETWORK_DHCP_RUN") and os.environ.get("reason"):
        return lifecycle.dhcp_hook(os.environ["WELD_NETWORK_DHCP_RUN"])
    return launch()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
        lifecycle.report(f"Network test stopped: {error}", error=True)
        sys.exit(1)
