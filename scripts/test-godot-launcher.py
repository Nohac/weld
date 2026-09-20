"""Check local-binary argument and environment forwarding without Godot or FHS."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

LAUNCHER = Path(__file__).with_name("run-godot")


class LauncherTests(unittest.TestCase):
    def test_wrapper_preserves_arguments_exit_status_and_rust_path(self):
        with tempfile.TemporaryDirectory(prefix="weld godot ") as temporary:
            directory = Path(temporary)
            steam = directory / "steam-run"
            steam.write_text(f"#!{sys.executable}\nimport json,os,sys\n"
                             "print(json.dumps([sys.argv[1:], os.environ['WELD_RUST_BUILD_PATH']]))\n"
                             "sys.exit(7)\n")
            steam.chmod(0o755)
            environment = dict(os.environ, WELD_GODOT_BIN=sys.executable,
                               PATH=str(directory) + os.pathsep + os.environ["PATH"])
            arguments = ["--path", "/project with spaces", "--", "literal;argument"]
            for inherited in (None, "/original/rust/tools"):
                environment.pop("WELD_RUST_BUILD_PATH", None)
                if inherited is not None:
                    environment["WELD_RUST_BUILD_PATH"] = inherited
                result = subprocess.run([shutil.which("bash"), str(LAUNCHER), *arguments],
                                        env=environment, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 7, result.stderr)
                forwarded, rust_path = json.loads(result.stdout)
                self.assertEqual(forwarded, [sys.executable, *arguments])
                self.assertEqual(rust_path, inherited or environment["PATH"])

    def test_explicit_missing_binary_does_not_fall_back_to_old_engine(self):
        environment = dict(os.environ, WELD_GODOT_BIN="/missing/weld-test-godot")
        result = subprocess.run([shutil.which("bash"), str(LAUNCHER), "--version"],
                                env=environment, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not executable", result.stderr)


if __name__ == "__main__":
    unittest.main()
