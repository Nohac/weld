#!/usr/bin/env python3
"""Pure workflow checks; no builds, device installation or network traffic."""
import importlib.machinery
import importlib.util
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest
from unittest.mock import patch

PATH = Path(__file__).with_name("run-godot-xr")
LOADER = importlib.machinery.SourceFileLoader("godot_xr_launcher", str(PATH))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
LOADER.exec_module(MODULE)


class WorkflowTests(unittest.TestCase):
    def test_export_builds_before_engine_checks_without_duplicate_build_step(self):
        steps = MODULE.steps()
        self.assertEqual([step.name for step in steps], ["format", "rust-tests", "clippy", "pico-export", "scene", "shaders"])
        self.assertTrue(all("--release" not in step.command for step in steps))

    def test_engine_zero_exit_with_error_or_missing_marker_is_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "test.log"
            step = MODULE.Step("scene", [], 1, "DONE", True)
            for output, code in [("SCRIPT ERROR: failed\nDONE\n", 0), ("\n", 0), ("DONE\n", 1)]:
                log.write_text(output)
                with self.assertRaises(RuntimeError):
                    MODULE.validate(step, code, log)
            log.write_text("DONE\n")
            MODULE.validate(step, 0, log)

    def test_failed_check_never_installs_or_launches(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(MODULE, "ROOT", Path(temporary)), \
             patch.object(MODULE, "APK", Path(temporary) / "build/test.apk"), \
             patch.object(MODULE.resource, "setrlimit"), \
             patch.object(MODULE.shutil, "which", return_value="/tools/adb"), \
             patch.object(MODULE.subprocess, "check_output", return_value="List of devices attached\npico\tdevice\n"), \
             patch.object(MODULE, "device_lock"), \
             patch.object(MODULE, "run_step", side_effect=RuntimeError("failed")) as run, \
             patch.object(MODULE, "launch_demo") as launch:
            with self.assertRaises(RuntimeError):
                MODULE.main([])
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[0].name, "format")
            launch.assert_not_called()

    def test_same_device_is_locked_until_workflow_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(MODULE.os.environ, {"XDG_RUNTIME_DIR": temporary}):
            with MODULE.device_lock("pico"):
                with self.assertRaises(RuntimeError):
                    MODULE.device_lock("pico")
                with MODULE.device_lock("phone"):
                    pass
            with MODULE.device_lock("pico"):
                pass

    def test_replacement_waits_for_previous_owner_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            environment = dict(MODULE.os.environ, XDG_RUNTIME_DIR=temporary)
            marker = Path(temporary) / "cleaned"
            child_code = """
import runpy, signal, sys, time
from pathlib import Path
api = runpy.run_path(sys.argv[1])
def stop(*unused):
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
with api['device_lock']('pico'):
    try:
        print('READY', flush=True)
        signal.pause()
    finally:
        time.sleep(0.15)
        Path(sys.argv[2]).touch()
"""
            child = subprocess.Popen([sys.executable, "-c", child_code, str(PATH), str(marker)],
                                     env=environment, stdout=subprocess.PIPE, text=True)
            try:
                self.assertEqual(child.stdout.readline().strip(), "READY")
                with patch.dict(MODULE.os.environ, {"XDG_RUNTIME_DIR": temporary}):
                    with MODULE.device_lock("pico"):
                        self.assertTrue(marker.exists(), "replacement started before cleanup")
                self.assertEqual(child.wait(timeout=5), 0)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=5)
                child.stdout.close()

    def test_recycled_owner_pid_is_not_signalled(self):
        with patch.object(MODULE.os, "pidfd_open", return_value=123), \
             patch.object(MODULE, "process_start", return_value=22), \
             patch.object(MODULE.Path, "stat") as info, \
             patch.object(MODULE.signal, "pidfd_send_signal") as send, \
             patch.object(MODULE.os, "close"):
            info.return_value.st_uid = MODULE.os.getuid()
            with self.assertRaises(RuntimeError):
                MODULE.request_stop(MODULE.os.getpid() + 100, 21)
            send.assert_not_called()


if __name__ == "__main__":
    unittest.main()
