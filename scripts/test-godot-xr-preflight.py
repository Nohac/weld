"""Preparation-cache and process-lifecycle tests; no builds or devices."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import runpy
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

HERE = Path(__file__).parent
API = runpy.run_path(str(HERE / "godot-xr-preflight.py"))
LOADER = importlib.machinery.SourceFileLoader("xr_preflight_test_launcher", str(HERE / "run-godot-xr"))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
LAUNCHER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = LAUNCHER
LOADER.exec_module(LAUNCHER)


class PreflightTests(unittest.TestCase):
    def test_build_environment_preserves_inherited_rust_path(self):
        original = {"PATH": "/caller", "WELD_RUST_BUILD_PATH": "/pinned"}
        self.assertEqual(LAUNCHER.build_environment(original)["PATH"], "/pinned")
        self.assertEqual(original["PATH"], "/caller")
        self.assertEqual(LAUNCHER.build_environment({"PATH": "/caller"})["WELD_RUST_BUILD_PATH"], "/caller")
        with self.assertRaises(RuntimeError):
            LAUNCHER.build_environment(dict(original, WELD_RUST_BUILD_PATH=""))

    def test_build_helper_restores_path_before_cargo_and_rejects_empty_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "apps/weld-vr"
            helper = project / "scripts/build-gdextension"
            helper.parent.mkdir(parents=True)
            shutil.copyfile(HERE.parent / "apps/weld-vr/scripts/build-gdextension", helper)
            library = project / "rust/target/x86_64-unknown-linux-gnu/debug/libweld_vr.so"
            library.parent.mkdir(parents=True)
            library.write_bytes(b"fixture")
            tools = root / "tools"
            tools.mkdir()
            cargo = tools / "cargo"
            cargo.write_text(f"#!{sys.executable}\nimport os,pathlib\npathlib.Path(os.environ['WELD_TEST_CAPTURE']).write_text(os.environ['PATH'])\n")
            cargo.chmod(0o755)
            capture = root / "path.txt"
            pinned = str(tools) + os.pathsep + os.environ["PATH"]
            env = dict(os.environ, PATH="/polluted", WELD_RUST_BUILD_PATH=pinned, WELD_TEST_CAPTURE=str(capture))
            command = [shutil.which("bash"), str(helper), "desktop"]
            subprocess.run(command, env=env, check=True, capture_output=True, timeout=5)
            self.assertEqual(capture.read_text(), pinned)
            env["WELD_RUST_BUILD_PATH"] = ""
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("must not be empty", result.stderr)

    def test_fingerprint_tracks_content_environment_outputs_and_engine_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "apps/weld-vr"
            (project / "scenes").mkdir(parents=True)
            source = project / "scenes/main.gd"
            source.write_text("first")
            sidecar = project / "scenes/main.gd.uid"
            sidecar.write_text("uid1")
            ignored = project / "build"
            ignored.mkdir()
            (ignored / "output.apk").write_text("ignored")
            environment = {"HOME": str(root), "PATH": "/tools"}
            def output(command, **kwargs):
                return b"" if command[0] == "git" else b"version1"
            with patch.object(API["subprocess"], "check_output", side_effect=output), \
                 patch.object(API["shutil"], "which", side_effect=lambda name, **kw: "/tools/" + name):
                fingerprint = lambda: API["fingerprint"](root, project, environment)
                first = fingerprint()
                (ignored / "output.apk").write_text("new output")
                self.assertEqual(fingerprint(), first)
                sidecar.write_text("uid2")
                rewritten = fingerprint()
                self.assertEqual(rewritten["inputs"], first["inputs"])
                self.assertNotEqual(rewritten["metadata"], first["metadata"])
                source.write_text("other")
                self.assertNotEqual(fingerprint()["inputs"], first["inputs"])
                before = fingerprint()
                environment["RUSTFLAGS"] = "-C opt-level=2"
                self.assertNotEqual(fingerprint(), before)
                environment.pop("RUSTFLAGS")
                source.unlink()
                self.assertNotEqual(fingerprint(), before)
                library = project / "bin/linux/debug/libweld_vr.so"
                library.parent.mkdir(parents=True)
                library.write_bytes(b"library")
                self.assertNotEqual(fingerprint(), before)

    def test_fingerprint_tracks_selected_tools_not_unrelated_path_entries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "apps/weld-vr"
            project.mkdir(parents=True)
            environment = {"HOME": str(root), "PATH": "/tools", "WELD_RUST_BUILD_PATH": "/tools"}
            with patch.object(API["subprocess"], "check_output", return_value=b"") as output, \
                 patch.object(API["shutil"], "which", side_effect=lambda name, **kw: "/tools/" + name) as which:
                fingerprint = lambda: API["fingerprint"](root, project, environment)
                first = fingerprint()
                environment.update(PATH="/unrelated:/tools", WELD_RUST_BUILD_PATH="/unrelated:/tools")
                self.assertEqual(fingerprint(), first)
                which.side_effect = lambda name, **kw: "/different-tools/" + name
                self.assertNotEqual(fingerprint(), first)
                which.side_effect = lambda name, **kw: "/tools/" + name
                output.side_effect = lambda command, **kw: b"new version" if command[0] == "rustc" else b""
                self.assertNotEqual(fingerprint(), first)

    def test_cache_requires_matching_inputs_and_apk_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache, apk = root / "cache.json", root / "viewer.apk"
            apk.write_bytes(b"apk")
            key = {"inputs": "source", "metadata": "uid"}
            self.assertFalse(API["cache_matches"](cache, key, apk))
            API["save_cache"](cache, key, apk)
            self.assertTrue(API["cache_matches"](cache, key, apk))
            self.assertFalse(API["cache_matches"](cache, dict(key, inputs="changed"), apk))
            apk.write_bytes(b"other")
            self.assertFalse(API["cache_matches"](cache, key, apk))
            apk.unlink()
            self.assertFalse(API["cache_matches"](cache, key, apk))
            cache.write_text("broken")
            self.assertFalse(API["cache_matches"](cache, key, apk))

    def test_gradle_template_sources_invalidate_but_generated_files_are_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "apps/weld-vr"
            template = project / "android/build"
            template.mkdir(parents=True)
            environment = {"HOME": str(root), "PATH": "/tools"}
            with patch.object(API["subprocess"], "check_output", return_value=b""), \
                 patch.object(API["shutil"], "which", side_effect=lambda name, **kw: "/tools/" + name):
                fingerprint = lambda: API["fingerprint"](root, project, environment)
                for name in ("config.gradle", "libs/debug/godot.aar", "src/main/AndroidManifest.xml"):
                    path = template / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    before = fingerprint()
                    path.write_bytes(b"new input")
                    self.assertNotEqual(fingerprint()["inputs"], before["inputs"])
                for name in ("libs/debug/arm64-v8a/libweld_vr.so", "libs/gdextensionlibs.json",
                             "src/debug/AndroidManifest.xml", "src/main/assets/project.binary", "res/values/themes.xml"):
                    path = template / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    before = fingerprint()
                    path.write_bytes(b"generated")
                    after = fingerprint()
                    self.assertEqual(after["inputs"], before["inputs"])
                    self.assertNotEqual(after["metadata"], before["metadata"])
                before = fingerprint()
                for name in ("build/outputs/app.apk", ".gradle/cache", "assetPackInstallTime/build/cache"):
                    path = template / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(b"ignored")
                self.assertEqual(fingerprint(), before)
                environment["XDG_SESSION_ID"] = "new-login"
                self.assertEqual(fingerprint(), before)

    def test_parallel_lanes_overlap_but_each_lane_keeps_order(self):
        barrier = threading.Barrier(2)
        calls = []
        def runner(step, directory, environment, *, cancelled):
            if step.endswith("1"):
                barrier.wait(timeout=2)
            calls.append(step)
        API["parallel_checks"]([["rust1", "rust2"], ["engine1", "engine2"]], runner, None, {})
        self.assertLess(calls.index("rust1"), calls.index("rust2"))
        self.assertLess(calls.index("engine1"), calls.index("engine2"))

    def test_failed_lane_cancels_and_reaps_sibling_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            child = LAUNCHER.Step("child", [sys.executable, "-c",
                "import os,time; print(os.getpid(),flush=True); time.sleep(30)"], 35)
            failing = LAUNCHER.Step("failure", [], 1)
            def runner(step, *args, **kwargs):
                if step is failing:
                    deadline = time.monotonic() + 3
                    while time.monotonic() < deadline:
                        if (directory / "child.log").exists() and (directory / "child.log").read_text().strip():
                            raise RuntimeError("intentional failure")
                        time.sleep(0.01)
                    raise RuntimeError("child failed to start")
                LAUNCHER.run_step(step, *args, **kwargs)
            start = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "intentional failure"):
                API["parallel_checks"]([[child], [failing]], runner, directory, dict(os.environ))
            self.assertLess(time.monotonic() - start, 3)
            pid = int((directory / "child.log").read_text().strip())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_timeout_reaps_owned_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            step = LAUNCHER.Step("timeout", [sys.executable, "-c",
                "import os,time; print(os.getpid(),flush=True); time.sleep(30)"], 0.2)
            with self.assertRaises(subprocess.TimeoutExpired):
                LAUNCHER.run_step(step, directory, dict(os.environ))
            with self.assertRaises(ProcessLookupError):
                os.kill(int((directory / "timeout.log").read_text().strip()), 0)

    def test_keyboard_interrupt_reaps_both_parallel_processes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            code = """
import os,runpy,sys
from pathlib import Path
api=runpy.run_path(sys.argv[1])
steps=[api['Step'](name,[sys.executable,'-c','import os,time; print(os.getpid(),flush=True); time.sleep(30)'],35)
       for name in ('one','two')]
api['PREFLIGHT']['parallel_checks']([[steps[0]],[steps[1]]],api['run_step'],Path(sys.argv[2]),dict(os.environ))
"""
            process = subprocess.Popen([sys.executable, "-c", code, str(HERE / "run-godot-xr"), temporary],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            descriptors = []
            try:
                deadline = time.monotonic() + 3
                logs = [directory / (name + ".log") for name in ("one", "two")]
                while not all(path.exists() and path.read_text().strip() for path in logs):
                    if time.monotonic() > deadline:
                        self.fail("parallel children did not start")
                    time.sleep(0.01)
                pids = [int(path.read_text().strip()) for path in logs]
                descriptors = [os.pidfd_open(pid) for pid in pids]
                process.send_signal(signal.SIGINT)
                self.assertNotEqual(process.wait(timeout=3), 0)
                for pid in pids:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)
            finally:
                for descriptor in descriptors:
                    try:
                        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    os.close(descriptor)
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=3)

    def test_cold_warm_recheck_failure_and_mid_run_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "target").mkdir()
            apk = root / "viewer.apk"
            apk.write_bytes(b"apk")
            key = {"inputs": "source", "metadata": "uid1"}
            def rewrite_metadata(*args):
                key["metadata"] = "uid2"
            with patch.object(LAUNCHER, "ROOT", root), patch.object(LAUNCHER, "APK", apk), \
                 patch.object(LAUNCHER, "run_step", side_effect=rewrite_metadata) as run, \
                 patch.dict(LAUNCHER.PREFLIGHT, fingerprint=lambda *args: dict(key),
                            parallel_checks=lambda *args: self.fail("launch must not run validation lanes")):
                args = SimpleNamespace(recheck=False)
                LAUNCHER.preparation(args, root, {"PATH": "/tools"})
                self.assertEqual([call.args[0].name for call in run.call_args_list], ["native-build", "pico-export"])
                run.reset_mock()
                LAUNCHER.preparation(args, root, {"PATH": "/tools"})
                self.assertEqual([call.args[0].name for call in run.call_args_list], ["native-build"])
                args.recheck = True
                run.reset_mock()
                LAUNCHER.preparation(args, root, {"PATH": "/tools"})
                self.assertEqual([call.args[0].name for call in run.call_args_list], ["native-build", "pico-export"])
                def edit_during_export(step, *unused):
                    if step.name == "pico-export":
                        key["inputs"] = "edited"
                with patch.object(LAUNCHER, "run_step", side_effect=edit_during_export):
                    with self.assertRaisesRegex(RuntimeError, "inputs changed"):
                        LAUNCHER.preparation(args, root, {"PATH": "/tools"})
                self.assertFalse((root / "target/godot-xr-preflight.json").exists())
                with patch.object(LAUNCHER, "run_step", side_effect=RuntimeError("failed")):
                    with self.assertRaisesRegex(RuntimeError, "failed"):
                        LAUNCHER.preparation(args, root, {"PATH": "/tools"})
                self.assertFalse((root / "target/godot-xr-preflight.json").exists())


if __name__ == "__main__":
    unittest.main()
