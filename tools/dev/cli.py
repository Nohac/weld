"""One output policy for every repository command."""

from collections.abc import Callable

import typer

FANCY_OUTPUT = False


def application(**kwargs) -> typer.Typer:
    return typer.Typer(
        rich_markup_mode="rich" if FANCY_OUTPUT else None,
        pretty_exceptions_enable=FANCY_OUTPUT,
        add_completion=False,
        context_settings={"color": FANCY_OUTPUT},
        **kwargs,
    )


def command(callback: Callable[..., object], *, help: str | None = None) -> typer.Typer:
    app = application()
    app.command(help=help)(callback)
    return app
