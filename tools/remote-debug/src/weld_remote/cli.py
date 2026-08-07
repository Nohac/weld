"""Command-line entry point for repeatable Weld remote-debug tasks."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from .client import RemoteDebugError, WeldRemoteClient


def parser() -> argparse.ArgumentParser:
    """Describe Weld's deliberately small remote-debug CLI."""

    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument("--url", default="http://127.0.0.1:15702/")
    root.add_argument("--timeout", type=float, default=10.0)
    commands = root.add_subparsers(dest="command", required=True)
    commands.add_parser("discover", help="list methods exposed by the endpoint")
    commands.add_parser("status", help="print Weld's capture status")
    screenshot = commands.add_parser(
        "screenshot", help="capture the complete nested composition"
    )
    screenshot.add_argument("path", type=Path)
    return root


def main() -> None:
    """Run one command and report failures without a Python traceback."""

    arguments = parser().parse_args()
    client = WeldRemoteClient(arguments.url, arguments.timeout)
    try:
        if arguments.command == "discover":
            result = client.discover()
        elif arguments.command == "status":
            result = client.status()
        else:
            result = client.screenshot(arguments.path)
        print(json.dumps(result, indent=2, sort_keys=True))
    except (RemoteDebugError, KeyError) as error:
        print(f"weld-debug: {error}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
