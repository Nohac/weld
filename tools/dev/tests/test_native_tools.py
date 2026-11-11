"""Native-tool selection and bounded build process ownership."""

import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from dev import build_android_ffmpeg, paths


class NativeTools(unittest.TestCase):
    def test_portal_python_skips_uv_environment(self):
        environment = {"PATH": f"{sys.prefix}/bin:/native/bin"}
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(
                paths.shutil, "which", return_value="/native/bin/python3"
            ) as which,
        ):
            self.assertEqual(paths.portal_python(), "/native/bin/python3")
        which.assert_called_once_with("python3", path="/native/bin")
        with patch.dict(os.environ, {"WELD_TEST_PORTAL_PYTHON": "/chosen/python"}):
            self.assertEqual(paths.portal_python(), "/chosen/python")

    def test_archive_checksum_rejects_corruption(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "archive"
            path.write_bytes(b"fixture")
            with patch.object(
                build_android_ffmpeg, "CHECKSUM", hashlib.sha256(b"fixture").hexdigest()
            ):
                build_android_ffmpeg.verify_archive(path)
                path.write_bytes(b"modified")
                with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                    build_android_ffmpeg.verify_archive(path)

    def test_timed_out_build_stops_compiler_children(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            log = directory / "build.log"
            program = (
                "import subprocess,sys,time; "
                "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
                "print(child.pid,flush=True); time.sleep(60)"
            )
            with self.assertRaises(subprocess.TimeoutExpired):
                build_android_ffmpeg.logged(
                    [sys.executable, "-c", program], log, cwd=directory, timeout=0.5
                )
            child_pid = int(log.read_text().strip())
            status = Path(f"/proc/{child_pid}/stat")
            # An orphan can briefly remain a zombie until the system reaps it.
            try:
                self.assertEqual(status.read_text().rsplit(")", 1)[1].split()[0], "Z")
            except FileNotFoundError:
                pass


if __name__ == "__main__":
    unittest.main()
