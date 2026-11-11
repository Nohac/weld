"""CLI contracts without a GPU, device, network, or spawned compositor."""

import importlib
import os
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

from dev import cli
from dev.paths import ROOT
from typer.testing import CliRunner


class Commands(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_registered_apps_have_plain_help_and_errors(self):
        scripts = tomllib.loads((ROOT / "pyproject.toml").read_text())["project"][
            "scripts"
        ]
        for name, entry in scripts.items():
            module, attribute = entry.split(":")
            app = getattr(importlib.import_module(module), attribute)
            with self.subTest(command=name):
                result = self.runner.invoke(app, ["--help"], color=True)
                self.assertEqual(result.exit_code, 0, result.output)
                self.assertIn("Usage:", result.output)
                self.assertNotIn("\x1b[", result.output)
                self.assertNotIn("╭", result.output)
                # Godot intentionally forwards unrecognized flags to the engine.
                if name == "run-godot":
                    continue
                result = self.runner.invoke(app, ["--not-a-weld-option"], color=True)
                self.assertEqual(result.exit_code, 2, result.output)
                self.assertIn("Error:", result.output)
                self.assertNotIn("\x1b[", result.output)
                self.assertNotIn("╭", result.output)

    def test_shared_toggle_controls_all_presentation_settings(self):
        with patch.object(cli, "FANCY_OUTPUT", True):
            app = cli.application()
        self.assertEqual(app.rich_markup_mode, "rich")
        self.assertTrue(app.pretty_exceptions_enable)
        assert app.info.context_settings is not None
        self.assertTrue(app.info.context_settings["color"])
        app = cli.application()
        self.assertIsNone(app.rich_markup_mode)
        self.assertFalse(app.pretty_exceptions_enable)
        assert app.info.context_settings is not None
        self.assertFalse(app.info.context_settings["color"])

    def test_invalid_modes_fail_before_external_work(self):
        cases = {
            "check_host_runtime": [
                ["--nested", "--shm-only"],
                ["--dormant", "--policy-only"],
            ],
            "check_control_launch": [["--attributed"], ["--text-app", "zenity"]],
            "run_connect": [
                ["--check", "--test"],
                ["--android", "--test"],
                ["--serial", "device"],
            ],
            "build_mobile": [["--jobs", "0"], ["--clippy", "--install"]],
            "paced_render": [["--mode", "invalid"], ["--focus-hz", "31"]],
        }
        with patch("subprocess.run", side_effect=AssertionError("unexpected process")):
            for name, cases in cases.items():
                app = importlib.import_module("dev." + name).app
                for arguments in cases:
                    with self.subTest(command=name, arguments=arguments):
                        result = self.runner.invoke(app, arguments)
                        self.assertEqual(
                            result.exit_code, 2, result.output or str(result.exception)
                        )

    def test_godot_forwards_arguments_and_preserves_build_path(self):
        from dev.run_godot import app

        with (
            patch.dict(
                os.environ,
                {"WELD_GODOT_BIN": "/test/godot", "PATH": "/test/bin"},
                clear=True,
            ),
            patch("os.access", return_value=True),
            patch("os.execvpe") as execute,
        ):
            result = self.runner.invoke(app, ["--", "--headless", "--path", "a path"])
        self.assertEqual(result.exit_code, 0, result.output)
        program, arguments, environment = execute.call_args.args
        self.assertEqual(program, "steam-run")
        self.assertEqual(
            arguments, ["steam-run", "/test/godot", "--headless", "--path", "a path"]
        )
        self.assertEqual(environment["WELD_RUST_BUILD_PATH"], "/test/bin")

    def test_connect_validation_does_not_package_or_launch(self):
        from dev import run_connect

        with (
            patch.object(run_connect, "run") as run,
            patch.object(
                run_connect, "dioxus_cli", side_effect=AssertionError("packaging")
            ),
            patch.object(
                run_connect.subprocess, "check_output", return_value="/test/lib\n"
            ),
            patch.object(Path, "mkdir"),
            patch.object(os, "umask"),
        ):
            result = self.runner.invoke(run_connect.app, ["--check", "--jobs", "3"])
        self.assertEqual(result.exit_code, 0, result.output or str(result.exception))
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[:2], ("cargo", "check"))
        self.assertIn("--locked", run.call_args.args)
        self.assertEqual(run.call_args.args[-2:], ("-j", 3))


if __name__ == "__main__":
    unittest.main()
