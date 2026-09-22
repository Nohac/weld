"""Focused launcher checks; no ADB or device changes."""
import importlib.machinery
import importlib.util
import subprocess
import tempfile
from pathlib import Path
import unittest
from unittest.mock import Mock, patch, call

loader = importlib.machinery.SourceFileLoader(
    "iroh_adb_probe", str(Path(__file__).with_name("run-iroh-adb-probe"))
)
spec = importlib.util.spec_from_loader(loader.name, loader)
probe = importlib.util.module_from_spec(spec)
loader.exec_module(probe)


class LauncherTests(unittest.TestCase):
    def test_reverse_uses_explicit_port_and_verifies_mapping(self):
        with (patch.object(probe.secrets, "randbelow", return_value=42),
              patch.object(probe, "run", side_effect=["", "", "UsbFfs tcp:61042 tcp:1234"]) as run):
            self.assertEqual(probe.create_reverse(["adb"], 1234), ("tcp:61042", 61042))
            self.assertIn(call(["adb", "reverse", "--no-rebind", "tcp:61042", "tcp:1234"]), run.call_args_list)

    def test_failed_reverse_creation_never_removes_a_mapping(self):
        failure = subprocess.CalledProcessError(1, "adb")
        with (patch.object(probe.secrets, "randbelow", side_effect=range(5)),
              patch.object(probe, "run", side_effect=[""] + [failure] * 5) as run):
            with self.assertRaises(RuntimeError):
                probe.create_reverse(["adb"], 1234)
            self.assertFalse(any("--remove" in item.args[0] for item in run.call_args_list))

    def test_reverse_cleanup_does_not_remove_replaced_mapping(self):
        with patch.object(probe, "run", return_value="UsbFfs tcp:61042 tcp:5678") as run:
            with self.assertRaisesRegex(RuntimeError, "changed ownership"):
                probe.remove_reverse(["adb"], "tcp:61042", 1234)
            run.assert_called_once_with(["adb", "reverse", "--list"])

    def test_ready_ignores_partial_line_and_validates_port(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output"
            path.write_text('noise\n{"event":"ready","port":1234}\n{')
            self.assertEqual(probe.ready(path, Mock()), 1234)
            path.write_text('{"event":"ready","port":0}\n')
            with self.assertRaises(RuntimeError):
                probe.ready(path, Mock())

    def test_failed_source_is_not_waited_on_forever(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output"
            path.write_text("failure")
            with self.assertRaisesRegex(RuntimeError, "source exited"):
                probe.ready(path, Mock(poll=Mock(return_value=1)))

    def test_stop_escalates_only_owned_process(self):
        process = Mock(poll=Mock(return_value=None))
        process.wait.side_effect = [subprocess.TimeoutExpired("probe", 3), 0]
        probe.stop(process)
        process.terminate.assert_called_once()
        process.kill.assert_called_once()
        self.assertEqual(process.wait.call_count, 2)

    def test_missing_measurements_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "output"
            path.write_text("")
            with self.assertRaisesRegex(RuntimeError, "missing phase"):
                probe.results(path, path, "raw")
            with self.assertRaisesRegex(RuntimeError, "authenticated"):
                probe.results(path, path, "iroh")

    def test_interrupt_and_client_failure_stop_both_owned_processes(self):
        for failure in (KeyboardInterrupt(), subprocess.TimeoutExpired("client", 50)):
            with self.subTest(failure=type(failure).__name__), tempfile.TemporaryDirectory() as directory:
                source, client = Mock(), Mock()
                client.wait.side_effect = failure
                with (patch.object(probe, "ROOT", Path(directory)),
                      patch("sys.argv", ["probe", "--desktop"]),
                      patch.object(probe.subprocess, "run"),
                      patch.object(probe, "run", return_value="disposable-id"),
                      patch.object(probe, "ready", return_value=1234),
                      patch.object(probe.subprocess, "Popen", side_effect=[source, client]),
                      patch.object(probe, "stop") as stop_owned,
                      patch("builtins.print")):
                    with self.assertRaises(type(failure)):
                        probe.main()
                    self.assertEqual(stop_owned.call_args_list, [call(client), call(source)])


if __name__ == "__main__":
    unittest.main()
