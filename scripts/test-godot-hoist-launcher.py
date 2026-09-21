"""Public pairing policy; process ownership is reused from the headless launcher."""
import json
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

API = runpy.run_path(str(Path(__file__).with_name("run-godot-hoist")))


class PairingTests(unittest.TestCase):
    def test_unlimited_runtime_keeps_source_and_network_summaries(self):
        self.assertEqual(API["parse_arguments"]([]).seconds, 120)
        self.assertIsNone(API["parse_arguments"](["--no-timeout"]).seconds)
        with self.assertRaises(SystemExit):
            API["parse_arguments"](["--no-timeout", "--seconds", "60"])
        with patch.dict(API["os"].environ, {"RUST_LOG": "warn"}, clear=True):
            for runtime in (Path("/test/runtime"), Path("/test/restart/runtime")):
                environment = API["source_environment"](runtime)
                self.assertEqual(environment["RUST_LOG"],
                                 "warn,weld_media_diag=debug,weld_vr_diag=debug,weld_network_diag=debug")
        with patch.dict(API["os"].environ, {"RUST_LOG": "warn,weld_media_diag=trace"}, clear=True):
            selected = API["diagnostic_environment"]()["RUST_LOG"]
            self.assertIn("weld_media_diag=trace", selected)
            self.assertNotIn("weld_media_diag=debug", selected)

    def test_gamepad_requires_explicit_source_opt_in_before_client_arguments(self):
        for enabled in (False, True):
            command = API["source_command"](Path("/run"), Path("/state"), ["azahar", "game.3ds"], gamepad=enabled)
            self.assertEqual("--hoist-gamepad" in command, enabled)
            if enabled:
                self.assertEqual(command[-4:], ["--hoist-gamepad", "--", "azahar", "game.3ds"])

    def test_azahar_rules_share_quality_group_with_catchall_last(self):
        azahar = runpy.run_path(str(Path(__file__).with_name("run-azahar-xr")))
        for manager in (False, True):
            document = json.loads(azahar["rules"](manager))
            rows = document["rules"]
            self.assertTrue(all(row["bitrate"]["group"] == 1 for row in rows))
            self.assertEqual([row["bitrate"]["role"] for row in rows], ["primary", "companion"] + (["utility"] if manager else []))
            self.assertTrue(rows[0]["title_suffix"] and rows[1]["title_suffix"])
            self.assertEqual([row["stereo"] for row in rows[:2]], [True, False])
            if manager:
                self.assertEqual(rows[-1]["title_suffix"], "")

    def test_source_keeps_host_audio_while_isolating_wayland_including_restart(self):
        original = {"XDG_RUNTIME_DIR": "/run/host", "DISPLAY": ":0",
                    "WAYLAND_DISPLAY": "wayland-1", "WAYLAND_SOCKET": "7"}
        with patch.dict(API["os"].environ, original, clear=True):
            for runtime in (Path("/test/runtime"), Path("/test/restart/runtime")):
                environment = API["source_environment"](runtime)
                self.assertEqual(environment["XDG_RUNTIME_DIR"], str(runtime))
                self.assertEqual(environment["PIPEWIRE_RUNTIME_DIR"], "/run/host")
                self.assertEqual(environment["PULSE_RUNTIME_PATH"], "/run/host/pulse")
                self.assertNotIn("PULSE_SERVER", environment)
                for name in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"):
                    self.assertNotIn(name, environment)
            self.assertEqual(dict(API["os"].environ), original)

    def test_source_preserves_explicit_audio_overrides(self):
        audio = {"PIPEWIRE_RUNTIME_DIR": "/custom/pipewire", "PIPEWIRE_REMOTE": "other",
                 "PULSE_RUNTIME_PATH": "/custom/pulse", "PULSE_SERVER": "unix:/custom/server",
                 "PULSE_SINK": "headphones"}
        with patch.dict(API["os"].environ, dict(audio, XDG_RUNTIME_DIR="/run/host"), clear=True):
            environment = API["source_environment"](Path("/test/runtime"))
            for name, value in audio.items():
                self.assertEqual(environment[name], value)

    def test_source_does_not_invent_host_audio_paths_without_runtime(self):
        with patch.dict(API["os"].environ, {}, clear=True):
            environment = API["source_environment"](Path("/test/runtime"))
            self.assertNotIn("PIPEWIRE_RUNTIME_DIR", environment)
            self.assertNotIn("PULSE_RUNTIME_PATH", environment)

    def test_azahar_game_path_is_one_argument_and_rules_are_one_shot(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            rom = root / "Mario (USA).3ds"
            rom.touch()
            args = API["parse_arguments"](["--app", "azahar", "--rom", str(rom)])
            self.assertEqual(API["app_command"](args), ["azahar", "--windowed", str(rom)])
            rules = root / "test.rules"
            rules.write_text(json.dumps({"rules": [{"app_id": "app", "title_suffix": "window",
                "stereo": True, "width": 1600, "height": 480, "slot": 0}]}))
            API["prepare_presentation_rules"](rules, directory=root)
            self.assertEqual((root / "presentation.rules").read_text(), rules.read_text())
            API["prepare_presentation_rules"](None, directory=root)
            self.assertFalse((root / "presentation.rules").exists())
    def test_codec_selection_preserves_av1_default_and_source_options(self):
        self.assertEqual(API["parse_arguments"]([]).codec, "av1")
        for codec in ("av1", "h264"):
            args = API["parse_arguments"](["--codec", codec])
            self.assertEqual(args.codec, codec)
            command = API["source_command"](Path("/run/test"), Path("/state"), ["blender"], 16, codec)
            self.assertEqual(command[command.index("--hoist-codec") + 1], codec)
            self.assertEqual(command[command.index("--hoist-bitrate-target-mbps") + 1], "16")

    def test_low_latency_is_explicit_android_only_and_clears_pending_marker(self):
        self.assertFalse(API["parse_arguments"]([]).decoder_low_latency)
        self.assertTrue(API["parse_arguments"](["--decoder-low-latency"]).decoder_low_latency)
        with self.assertRaises(SystemExit):
            API["parse_arguments"](["--desktop", "--decoder-low-latency"])
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            marker = directory / API["LOW_LATENCY_MARKER"]
            API["prepare_decoder_test"](True, directory=directory)
            self.assertTrue(marker.exists())
            API["prepare_decoder_test"](False, directory=directory)
            self.assertFalse(marker.exists())
        with patch.object(API["subprocess"], "run") as run:
            API["prepare_decoder_test"](True, adb=["adb", "-s", "pico"])
            self.assertEqual(run.call_args.args[0][-2:], ["touch", "files/weld-device/diagnostic-low-latency"])
            API["prepare_decoder_test"](False, adb=["adb", "-s", "pico"])
            self.assertEqual(run.call_args.args[0][-3:], ["rm", "-f", "files/weld-device/diagnostic-low-latency"])

    def test_half_rate_is_explicit_and_normal_launch_clears_pending_test(self):
        self.assertFalse(API["parse_arguments"]([]).half_rate)
        self.assertTrue(API["parse_arguments"](["--half-rate"]).half_rate)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            marker = directory / API["HALF_RATE_MARKER"]
            API["prepare_cadence_test"](True, directory=directory)
            self.assertTrue(marker.exists())
            API["prepare_cadence_test"](False, directory=directory)
            self.assertFalse(marker.exists())
        with patch.object(API["subprocess"], "run") as run:
            API["prepare_cadence_test"](True, adb=["adb", "-s", "pico"])
            self.assertEqual(run.call_args.args[0][-2:], ["touch", "files/weld-device/diagnostic-half-rate"])
            API["prepare_cadence_test"](False, adb=["adb", "-s", "pico"])
            self.assertEqual(run.call_args.args[0][-3:], ["rm", "-f", "files/weld-device/diagnostic-half-rate"])

    def test_blender_exercise_is_explicit_and_does_not_load_user_startup(self):
        script = Path(__file__).resolve()
        args = API["parse_arguments"](["--app", "blender", "--blender-script", str(script)])
        self.assertEqual(API["app_command"](args),
                         ["blender", "--factory-startup", "--python", str(script)])
        self.assertEqual(API["app_command"](API["parse_arguments"](["--app", "blender"])), ["blender"])
        with self.assertRaises(SystemExit):
            API["parse_arguments"](["--blender-script", str(script)])

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
