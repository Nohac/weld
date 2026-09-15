"""Public pairing policy; process ownership is reused from the headless launcher."""
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

API = runpy.run_path(str(Path(__file__).with_name("run-godot-hoist")))


class PairingTests(unittest.TestCase):
    def test_demo_bitrate_is_shared_explicit_and_defaults_to_sixteen(self):
        self.assertEqual(API["parse_arguments"]([]).bitrate_mbps, 16)
        self.assertEqual(API["parse_arguments"](["--bitrate-mbps", "24"]).bitrate_mbps, 24)
        for bitrate in (8, 16, 24):
            command = API["source_command"](Path("/run/test"), Path("/state"), ["blender"], bitrate)
            self.assertEqual(command[command.index("--hoist-bitrate-target-mbps") + 1], str(bitrate))

    def test_source_uses_explicit_repeat_with_legacy_emulation(self):
        command = API["source_command"](Path("/run/test"), Path("/state"), ["blender"])
        self.assertEqual(command[command.index("--keyboard-repeat-mode") + 1], "compositor")
        self.assertEqual(command[command.index("--legacy-key-repeat") + 1], "emulated")

    def test_android_process_may_appear_after_saved_identity(self):
        with patch.object(API["subprocess"], "run", side_effect=[
            subprocess.CompletedProcess([], 1, "", ""),
            subprocess.CompletedProcess([], 0, "13766\n", ""),
        ]):
            self.assertIsNone(API["viewer_process"](["adb", "-s", "test-device"]))
            self.assertEqual(API["viewer_process"](["adb", "-s", "test-device"]), "13766")

    def test_android_process_probe_rejects_ambiguous_or_failed_results(self):
        for result, error in (
            (subprocess.CompletedProcess([], 0, "12 34\n", ""), RuntimeError),
            (subprocess.CompletedProcess([], 2, "", "device offline"), subprocess.CalledProcessError),
        ):
            with self.subTest(result=result), patch.object(API["subprocess"], "run", return_value=result):
                with self.assertRaises(error):
                    API["viewer_process"](["adb"])

    def test_failed_restart_close_keeps_source_owned_for_cleanup(self):
        source = Mock(role="source")
        source.close.side_effect = [RuntimeError("close failed"), None]
        hosts = [source]
        with self.assertRaises(RuntimeError):
            API["close_source_for_restart"](hosts)
        self.assertEqual(hosts, [source])
        API["stop_owned"](hosts)
        self.assertEqual(source.close.call_count, 2)

    def test_cleanup_attempts_all_owners_and_reports_each_failure(self):
        for phone_fails, host_fails in ((True, False), (False, True), (True, True)):
            with self.subTest(phone_fails=phone_fails, host_fails=host_fails):
                phone = Mock(side_effect=RuntimeError("phone failed") if phone_fails else None)
                source = Mock(role="source")
                viewer = Mock(role="viewer")
                source.close.side_effect = RuntimeError("host failed") if host_fails else None
                with self.assertRaisesRegex(RuntimeError, "Cleanup incomplete") as result:
                    API["stop_owned"]([source, viewer], phone)
                phone.assert_called_once()
                source.close.assert_called_once()
                viewer.close.assert_called_once()
                self.assertEqual("phone failed" in str(result.exception), phone_fails)
                self.assertEqual("host failed" in str(result.exception), host_fails)

    def test_restart_keeps_private_identity_but_uses_fresh_publication_paths(self):
        first = API["source_command"](Path("/run/one"), Path("/state"), ["foot"])
        second = API["source_command"](Path("/run/two"), Path("/state"), ["foot"])
        for command in (first, second):
            self.assertEqual(command[command.index("--hoist-iroh-device-dir") + 1], "/state/source")
            self.assertIn("n0", command)
        self.assertNotEqual(first[first.index("--hoist-iroh-publish-profile") + 1],
                            second[second.index("--hoist-iroh-publish-profile") + 1])

    def test_identity_is_public_canonical_and_bounded(self):
        value = "ab" * 32
        self.assertEqual(API["identity"](value + "\n"), value)
        for invalid in ("", "g" * 64, value + "\n" + value, "a" * 4097):
            with self.assertRaises(RuntimeError):
                API["identity"](invalid)

    def test_existing_source_pin_cannot_silently_change(self):
        profile = "peer=" + "ab" * 32 + "\nnetwork=n0\n"
        API["check_existing"](None, profile)
        API["check_existing"](profile, profile + "address=127.0.0.1:42\n")
        with self.assertRaises(RuntimeError):
            API["check_existing"](profile, profile.replace("ab", "cd"))
        for invalid in (profile + profile, "network=n0\n", profile + " " * 4096):
            with self.assertRaises(RuntimeError):
                API["profile_peer"](invalid)

    def test_publication_never_replaces_existing_file(self):
        with tempfile.TemporaryDirectory(prefix="weld-godot-pair-") as temporary:
            path = Path(temporary) / "viewer.identity"
            API["write_new"](path, "ab" * 32)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                API["write_new"](path, "cd" * 32)
            self.assertEqual(path.read_text(), "ab" * 32)


if __name__ == "__main__":
    unittest.main()
