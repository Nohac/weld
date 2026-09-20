"""SDK compatibility tests without Nix writes, Godot, downloads or a device."""
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

API = runpy.run_path(str(Path(__file__).with_name("godot-android-env.py")))


class SdkTests(unittest.TestCase):
    def test_alias_and_private_settings_preserve_source_and_align_gradle(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sdk = root / "nix-sdk"
            for version in ("9.0", "22.0"):
                tool = sdk / "cmdline-tools" / version / "bin/sdkmanager"
                tool.parent.mkdir(parents=True)
                tool.touch()
            (sdk / "platform-tools").mkdir()
            settings = root / "original-config/godot/editor_settings-4.8.tres"
            settings.parent.mkdir(parents=True)
            original = '[gd_resource type="EditorSettings" format=3]\n[resource]\nexport/android/android_sdk_path = "/old"\nexport/android/debug_keystore = "/keep/key"\n'
            settings.write_text(original)
            environment = {"ANDROID_HOME": str(sdk), "ANDROID_SDK_ROOT": "/old",
                           "XDG_CONFIG_HOME": str(settings.parent.parent), "PATH": "/unchanged"}
            # Exercise the Nix-only branch with an isolated, writable fixture.
            with patch.object(Path, "is_relative_to", return_value=True):
                result = API["export_environment"](root, environment)
                again = API["export_environment"](root, environment)
            view = Path(result["ANDROID_HOME"])
            self.assertEqual(result, again)
            self.assertEqual(result["ANDROID_SDK_ROOT"], str(view))
            self.assertEqual((view / "cmdline-tools/latest").resolve(), sdk / "cmdline-tools/22.0")
            self.assertEqual((view / "platform-tools").resolve(), sdk / "platform-tools")
            copy = Path(result["XDG_CONFIG_HOME"]) / "godot" / settings.name
            self.assertIn(str(view), copy.read_text())
            self.assertIn('debug_keystore = "/keep/key"', copy.read_text())
            self.assertEqual(copy.stat().st_mode & 0o777, 0o600)
            self.assertEqual(settings.read_text(), original)
            self.assertFalse((sdk / "cmdline-tools/latest").exists())
            self.assertEqual(environment["ANDROID_SDK_ROOT"], "/old")
            self.assertEqual(result["PATH"], "/unchanged")
            settings.write_text(original.replace("/keep/key", "/new/key"))
            with patch.object(Path, "is_relative_to", return_value=True):
                API["export_environment"](root, environment)
            self.assertIn('debug_keystore = "/new/key"', copy.read_text())

    def test_other_sdk_layouts_are_not_changed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            environment = {"ANDROID_HOME": str(root / "sdk")}
            self.assertEqual(API["export_environment"](root, environment), environment)
            self.assertEqual(API["export_environment"](root, {}), {})
            (root / "sdk/cmdline-tools/latest").mkdir(parents=True)
            with patch.object(Path, "is_relative_to", return_value=True):
                self.assertEqual(API["export_environment"](root, environment), environment)
            self.assertFalse((root / "target").exists())

    def test_existing_unexpected_entry_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "entry"
            path.write_text("preserve")
            with self.assertRaisesRegex(RuntimeError, "refusing to replace"):
                API["link"](path, Path(temporary))
            self.assertEqual(path.read_text(), "preserve")


if __name__ == "__main__":
    unittest.main()
