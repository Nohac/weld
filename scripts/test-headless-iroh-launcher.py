"""No GPU/network required: command policy and owned process-group cleanup."""
import os
import json
from pathlib import Path
import runpy
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

LAUNCHER = runpy.run_path(str(Path(__file__).with_name("run-headless-iroh-hoist")))


class LauncherTests(unittest.TestCase):
    def test_full_launcher_cancel_and_receiver_exit_cleanup(self):
        for mode in ("cancel-before-pairing", "cancel-running", "receiver-exits", "receiver-fails"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(prefix="weld-launcher-flow-") as directory:
                root = Path(directory)
                scripts, binaries = root / "scripts", root / "bin"
                scripts.mkdir()
                binaries.mkdir()
                launcher = scripts / "run-headless-iroh-hoist"
                shutil.copy2(Path(__file__).with_name("run-headless-iroh-hoist"), launcher)
                fixture = Path(__file__).with_name("fixtures") / "headless-iroh-cargo.py"
                for name in ("cargo", "foot", "htop"):
                    (binaries / name).symlink_to(fixture.resolve())
                environment = {**os.environ, "PATH": str(binaries) + os.pathsep + os.environ["PATH"],
                               "WAYLAND_DISPLAY": "fake", "WELD_LAUNCHER_TEST_MODE": mode}
                environment.pop("RUST_LOG", None)
                if mode == "receiver-fails":
                    environment["RUST_LOG"] = "error"
                with (root / "launcher.log").open("w+") as log:
                    parent = subprocess.Popen([sys.executable, str(launcher), "--app", "foot",
                                               "--codec", "h264"], env=environment,
                                              stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
                    try:
                        deadline = time.monotonic() + 10
                        if mode.startswith("cancel"):
                            while True:
                                runs = list((root / "target/validation").glob("headless-iroh-*"))
                                ready = (runs and (runs[0] / "fake-source-ready").exists()
                                         if mode == "cancel-before-pairing"
                                         else "Both Weld instances ready" in (root / "launcher.log").read_text())
                                if ready:
                                    break
                                self.assertIsNone(parent.poll())
                                self.assertLess(time.monotonic(), deadline)
                                time.sleep(0.02)
                            parent.send_signal(signal.SIGINT)
                        result = parent.wait(timeout=12)
                        self.assertEqual(result, 1 if mode == "receiver-fails" else 0,
                                         (root / "launcher.log").read_text())
                    finally:
                        if parent.poll() is None:
                            parent.kill()  # Supervisors observe EOF and clean their own groups.
                            parent.wait(timeout=5)
                    run = next((root / "target/validation").glob("headless-iroh-*"))
                    metadata = json.loads((run / "run.json").read_text())
                    self.assertEqual((metadata["codec"], metadata["transport"], metadata["receiver"]),
                                     ("h264", "direct", "weld"))
                    for owned in json.loads((run / "processes.json").read_text()):
                        expected_log = "error" if mode == "receiver-fails" else "info,weld_media_diag=debug,weld_network_diag=debug"
                        self.assertIn("TEST_RUST_LOG=" + expected_log,
                                      (run / (owned["role"] + ".log")).read_text())
                        with self.assertRaises(ProcessLookupError):
                            os.kill(owned["pid"], 0)

    def test_commands_keep_sources_headless_and_receivers_nested(self):
        args = LAUNCHER["parse_arguments"]([])
        self.assertIsNone(args.seconds)
        self.assertEqual(args.codec, "av1")
        self.assertEqual(args.network, "direct")
        source, destination = LAUNCHER["commands"](args, Path("/private/run"))
        self.assertIn("--hoist-all", source)
        self.assertIn("headless", source)
        self.assertIn("nested", destination)
        self.assertNotIn("--hoist-all", destination)
        self.assertIn("emulated", source)
        self.assertEqual(source[source.index("--hoist-iroh-listen") + 1],
                         destination[destination.index("--hoist-iroh-connect") + 1])
        apps = LAUNCHER["applications"](["foot", "firefox", "foot"], Path("/private/profile"))
        self.assertEqual(apps, [["foot", "-e", "htop"],
                                ["firefox", "--no-remote", "--profile", "/private/profile"]])

    def test_cleanup_kills_descendants_even_after_host_exits(self):
        with tempfile.TemporaryDirectory(prefix="weld-launcher-test-") as directory:
            run = Path(directory)
            child_pid = run / "descendant.pid"
            command = [sys.executable, "-c",
                       "import subprocess,sys; from pathlib import Path; "
                       "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
                       "Path(sys.argv[1]).write_text(str(p.pid))", str(child_pid)]
            host = LAUNCHER["OwnedHost"]("test", command, os.environ.copy(), run, run)
            try:
                deadline = time.monotonic() + 5
                while not host.exited():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.02)
                descendant = int(child_pid.read_text())
                self.assertEqual(os.getpgid(descendant), host.process.pid)
                self.assertIsNone(host.process.poll(), "supervisor must pin the group after Weld exits")
            finally:
                host.close()
            self.assertIsNotNone(host.process.poll())
            # A descendant can briefly remain a zombie until its new parent reaps it.
            stat = Path(f"/proc/{descendant}/stat")
            if stat.exists():
                self.assertEqual(stat.read_text().rsplit(")", 1)[1].split()[0], "Z")

    def test_owned_host_resets_inherited_shutdown_mask(self):
        with tempfile.TemporaryDirectory(prefix="weld-launcher-mask-") as directory:
            run = Path(directory)
            old = signal.pthread_sigmask(signal.SIG_BLOCK, LAUNCHER["SHUTDOWN_SIGNALS"])
            try:
                host = LAUNCHER["OwnedHost"]("signals", [sys.executable, "-c",
                    "import signal; assert not signal.pthread_sigmask(signal.SIG_BLOCK, [])"],
                    os.environ.copy(), run, run)
            finally:
                signal.pthread_sigmask(signal.SIG_SETMASK, old)
            try:
                deadline = time.monotonic() + 5
                while not host.exited():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.02)
                self.assertIn('"exit_code": 0', host.status.read_text())
            finally:
                host.close()


if __name__ == "__main__":
    unittest.main()
