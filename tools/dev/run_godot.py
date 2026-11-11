"""Run the selected Godot binary while preserving Rust build-tool selection."""

import os
from pathlib import Path

import typer

from dev.cli import application

app = application()


@app.command(
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True}
)
def main(ctx: typer.Context):
    """Forward arguments to Godot; use -- before Godot's own flags."""
    environment = dict(os.environ)
    environment.setdefault("WELD_RUST_BUILD_PATH", environment.get("PATH", ""))
    explicit = environment.get("WELD_GODOT_BIN")
    binary = Path(explicit) if explicit else Path.home() / ".local/bin/godot"
    command = ["godot"]
    if explicit or os.access(binary, os.X_OK):
        if not os.access(binary, os.X_OK):
            raise typer.BadParameter(f"Godot binary is not executable: {binary}")
        command = ["steam-run", str(binary)]
    os.execvpe(command[0], [*command, *ctx.args], environment)


if __name__ == "__main__":
    app()
