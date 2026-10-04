#!/usr/bin/env python3
"""Owned-process cleanup regression checks; no hardware or subprocesses."""
from pathlib import Path
import runpy
import subprocess
import unittest
from unittest.mock import Mock

STOP = runpy.run_path(str(Path(__file__).with_name("run-mobile-hoist")))["stop_run"]


class CleanupTests(unittest.TestCase):
    def test_stalled_log_capture_is_killed_before_remaining_cleanup(self):
        capture = Mock()
        capture.wait.side_effect = [subprocess.TimeoutExpired("logcat", 5), 0]
        viewer = Mock()
        host = Mock()
        STOP(capture, viewer, host)
        capture.terminate.assert_called_once()
        capture.kill.assert_called_once()
        viewer.assert_called_once()
        host.close.assert_called_once()

    def test_capture_and_device_failure_still_close_host(self):
        capture = Mock()
        capture.terminate.side_effect = OSError("capture failed")
        viewer = Mock(side_effect=OSError("device unplugged"))
        host = Mock()
        with self.assertRaisesRegex(OSError, "device unplugged"):
            STOP(capture, viewer, host)
        viewer.assert_called_once()
        host.close.assert_called_once()


if __name__ == "__main__":
    unittest.main()
