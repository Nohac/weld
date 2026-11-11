#!/usr/bin/env python3
"""Import pinned i3 parser states for the one-time grammar bootstrap and audit."""

import hashlib
import json
import re
from typing import Annotated

import typer

from dev.cli import command
from dev.paths import ROOT

DIRECTORY = ROOT / "tools/config-codegen"


def import_states():
    sources = json.loads((DIRECTORY / "upstream/sources.json").read_text())
    imported = {"revision": sources["revision"], "states": {}}
    for entry in sources["files"]:
        path = DIRECTORY / "upstream" / entry["file"]
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != entry["sha256"]:
            raise ValueError(f"upstream checksum mismatch: {path}")
        if path.suffix != ".spec":
            continue
        state = None
        pending = []
        for line_number, line in enumerate(data.decode().splitlines(), 1):
            text = line.strip()
            if not text or text.startswith("#"):
                continue
            if text.startswith("state "):
                if pending:
                    raise ValueError(
                        f"unfinished production before {path}:{line_number}"
                    )
                state = f"{path.name}:{text[6:-1]}"
                if state in imported["states"]:
                    raise ValueError(f"duplicate state {state}")
                imported["states"][state] = {"line": line_number, "productions": []}
                continue
            if state is None:
                raise ValueError(f"production outside state at {path}:{line_number}")
            pending.append(text)
            if "->" not in text:
                continue
            pattern, action = " ".join(pending).split("->", 1)
            imported["states"][state]["productions"].append(
                {
                    "pattern": pattern.strip(),
                    "action": action.strip(),
                    "callbacks": re.findall(r"\bcall\s+(\w+)", action),
                }
            )
            pending.clear()
        if pending:
            raise ValueError(f"unfinished production at end of {path}")
    return imported


def main(
    check: Annotated[bool, typer.Option("--check")] = False,
    seed: Annotated[bool, typer.Option("--seed")] = False,
):
    imported = import_states()
    output = json.dumps(imported, indent=2) + "\n"
    destination = DIRECTORY / "i3.json"
    if check:
        if not destination.exists() or destination.read_text() != output:
            raise SystemExit("stale or modified i3 bootstrap inventory")
    else:
        destination.write_text(output)
    if seed:
        if check:
            raise ValueError("--seed and --check are mutually exclusive")
        definition = DIRECTORY / "sway.json"
        schema = json.loads(definition.read_text())
        direction = next(
            production
            for production in imported["states"]["commands.spec:FOCUS"]["productions"]
            if production["action"] == "call cmd_focus_direction($direction)"
        )
        schema["enums"]["Direction"] = {
            spelling.title(): [spelling]
            for spelling in re.findall("'([^']+)'", direction["pattern"])
        }
        definition.write_text(json.dumps(schema, indent=2) + "\n")


app = command(main, help=__doc__)

if __name__ == "__main__":
    app()
