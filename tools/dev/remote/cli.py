"""Restricted development RPC client."""

import json
from pathlib import Path
from typing import Annotated

import typer

from dev.cli import application

from .client import RemoteDebugError, WeldRemoteClient

app = application(no_args_is_help=True)


@app.callback()
def configure(
    ctx: typer.Context,
    url: Annotated[str, typer.Option("--url")] = "http://127.0.0.1:15702/",
    timeout: Annotated[float, typer.Option("--timeout", min=0.001)] = 10.0,
):
    """Inspect or capture a Weld development endpoint."""
    ctx.obj = WeldRemoteClient(url, timeout)


def output(operation, *arguments):
    try:
        print(json.dumps(operation(*arguments), indent=2, sort_keys=True))
    except (RemoteDebugError, KeyError) as error:
        typer.echo(f"weld-debug: {error}", err=True)
        raise typer.Exit(1) from error


@app.command()
def discover(ctx: typer.Context):
    """List methods exposed by the endpoint."""
    output(ctx.obj.discover)


@app.command()
def status(ctx: typer.Context):
    """Print capture status."""
    output(ctx.obj.status)


@app.command()
def screenshot(ctx: typer.Context, path: Path):
    """Capture the complete nested composition."""
    output(ctx.obj.screenshot, path)


if __name__ == "__main__":
    app()
