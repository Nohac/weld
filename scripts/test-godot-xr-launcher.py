#!/usr/bin/env python3
"""Pure workflow checks; no builds, device installation or network traffic."""
import importlib.machinery
import importlib.util
from pathlib import Path
import sys
import subprocess
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

PATH = Path(__file__).with_name("run-godot-xr")
LOADER = importlib.machinery.SourceFileLoader("godot_xr_launcher", str(PATH))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
LOADER.exec_module(MODULE)


class WorkflowTests(unittest.TestCase):
    def test_low_latency_flag_is_forwarded_without_changing_other_preferences(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(MODULE, "ROOT", Path(temporary)), \
             patch.object(MODULE, "APK", Path(temporary) / "viewer.apk"), \
             patch.object(MODULE.shutil, "which", return_value="/tools/tool"), \
             patch.object(MODULE, "preparation") as prepare, \
             patch.object(MODULE, "run_step"), \
             patch.object(MODULE, "launch_demo") as launch:
            MODULE.APK.touch()
            for enabled, seconds in ((False, 75), (True, None)):
                args = SimpleNamespace(app="blender", seconds=seconds, bitrate_mbps=16, codec="h264",
                                       half_rate=False, decoder_low_latency=enabled, recheck=False)
                MODULE.workflow(args, "/external/adb", "pico", {"PATH": "/tools"})
                self.assertEqual(prepare.call_args.args[2]["PATH"], "/tools")
                self.assertEqual(launch.call_args.args[1]["PATH"], "/external:/tools")
                command = launch.call_args.args[0]
                self.assertEqual("--decoder-low-latency" in command, enabled)
                self.assertNotIn("--half-rate", command)
                self.assertEqual(command[command.index("--bitrate-mbps") + 1], "16")
                self.assertEqual(command[command.index("--codec") + 1], "h264")
                self.assertEqual("--no-timeout" in command, seconds is None)
                self.assertEqual("--seconds" in command, seconds is not None)
                if seconds is not None:
                    self.assertEqual(command[command.index("--seconds") + 1], "75")

    def test_launch_only_exports_and_validation_is_separate(self):
        steps = MODULE.steps()
        self.assertEqual([step.name for step in steps], ["pico-export"])
        self.assertTrue(all("--release" not in step.command for step in steps))
        checks = runpy.run_path(str(PATH.with_name("check-godot-xr")))["steps"]()
        self.assertEqual([step.name for step in checks], ["format", "rust-tests", "clippy", "scene", "shaders"])
        for step in checks:
            if step.name in ("rust-tests", "clippy"):
                self.assertEqual(step.command[step.command.index("--target") + 1], "x86_64-unknown-linux-gnu")

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
             patch.object(MODULE, "preparation_lock"), \
             patch.object(MODULE, "preparation", side_effect=RuntimeError("failed")), \
             patch.object(MODULE, "run_step") as run, \
             patch.object(MODULE, "launch_demo") as launch:
            with self.assertRaises(RuntimeError):
                MODULE.main([])
            run.assert_not_called()
            launch.assert_not_called()

    def test_only_exact_preview_shutdown_diagnostic_after_successful_export_is_accepted(self):
        banner = "Godot Engine v4.8.dev6.official.8898c2b3d\n"
        complete = "[ 99% ] export | Build complete.\n"
        diagnostic = 'ERROR: EditorSettings not instantiated yet when getting setting "export/android/shutdown_adb_on_exit".\n'
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "export.log"
            step = MODULE.Step("pico-export", [], 1, engine=True)
            log.write_text(banner + complete + diagnostic)
            MODULE.validate(step, 0, log)
            for text, code in [(banner + complete + diagnostic, 1),
                               (banner + diagnostic, 0),
                               (complete + diagnostic, 0),
                               (banner.replace("dev6", "dev7") + complete + diagnostic, 0),
                               (banner + complete + diagnostic + "ERROR: another failure\n", 0),
                               (banner + diagnostic + complete, 0)]:
                log.write_text(text)
                with self.assertRaises(RuntimeError):
                    MODULE.validate(step, code, log)
            log.write_text(banner + complete + diagnostic)
            with self.assertRaises(RuntimeError):
                MODULE.validate(MODULE.Step("scene", [], 1, engine=True), 0, log)

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
