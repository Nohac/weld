#!/usr/bin/env python3
"""Launcher test double: files instead of GPU, Wayland or Iroh resources."""
import os
from pathlib import Path
import sys
import time

if sys.argv[1] == "build":
    sys.exit(0)

args = sys.argv[1:]
mode = os.environ["WELD_LAUNCHER_TEST_MODE"]
source = "--hoist-iroh-listen" in args
if source:
    ticket = Path(args[args.index("--hoist-iroh-listen") + 1])
    (ticket.parent / "fake-source-ready").touch()
    if mode != "cancel-before-pairing":
        ticket.touch()
else:
    Path(args[args.index("--wayland-socket") + 1]).touch()
    if mode in ("receiver-exits", "receiver-fails"):
        time.sleep(0.2)
        sys.exit(0 if mode == "receiver-exits" else 7)
while True:
    time.sleep(1)
