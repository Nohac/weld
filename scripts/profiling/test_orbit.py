"""Safety and IPC checks for the bounded orbit diagnostic; no display required."""

from importlib.machinery import SourceFileLoader
from importlib.util import module_from_spec, spec_from_loader
from pathlib import Path
import struct
import subprocess
import unittest
from unittest.mock import Mock, patch


loader = SourceFileLoader('orbit', str(Path(__file__).with_name('orbit')))
spec = spec_from_loader(loader.name, loader)
orbit = module_from_spec(spec)
loader.exec_module(orbit)


class OrbitTests(unittest.TestCase):
    def test_drm_launcher_rejects_non_tty_before_starting_tools(self):
        result = subprocess.run(
            [str(Path(__file__).with_name('drm-orbit'))],
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn('run from a text TTY', result.stderr)

    def test_prepared_perf_binary_requires_no_build(self):
        result = subprocess.run(
            [str(Path(__file__).with_name('run-perf')), '--name', 'test',
             '--instructions', 'test', '--binary', '/not-a-binary'],
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn('--binary requires --no-build', result.stderr)

    def test_abort_explicitly_releases_middle_before_closing_helper(self):
        pointer = Mock()
        orbit.close_pointer(pointer, True)
        self.assertEqual(pointer.mock_calls, [
            unittest.mock.call.stdin.write('b 0\n'),
            unittest.mock.call.stdin.flush(),
            unittest.mock.call.stdin.close(),
            unittest.mock.call.wait(timeout=3),
        ])

    def test_failed_release_still_closes_pipe(self):
        pointer = Mock()
        pointer.stdin.write.side_effect = BrokenPipeError
        with self.assertRaises(BrokenPipeError):
            orbit.close_pointer(pointer, True)
        pointer.stdin.close.assert_called_once()

    def test_fragmented_ipc_reply(self):
        sway = object.__new__(orbit.Sway)
        payload = b'[{"success":true}]'
        header = b'i3-ipc' + struct.pack('=II', len(payload), 0)
        sway.connection = Mock()
        sway.connection.recv.side_effect = [header[:3], header[3:], payload[:5], payload[5:]]
        sway.command('focus')
        sway.connection.sendall.assert_called_once_with(b'i3-ipc' + struct.pack('=II', 5, 0) + b'focus')

    def test_oversized_ipc_reply_rejected_before_body(self):
        sway = object.__new__(orbit.Sway)
        sway.connection = Mock()
        sway.connection.recv.return_value = b'i3-ipc' + struct.pack('=II', 17 * 1024 * 1024, 4)
        with self.assertRaisesRegex(RuntimeError, 'Invalid Sway'):
            sway.request(4)
        self.assertEqual(sway.connection.recv.call_count, 1)

    def test_ipc_disconnect_stops_read(self):
        sway = object.__new__(orbit.Sway)
        sway.connection = Mock()
        sway.connection.recv.return_value = b''
        with self.assertRaisesRegex(RuntimeError, 'disconnected'):
            sway.receive(14)

    def test_focus_guard_includes_floating_windows(self):
        sway = Mock()
        sway.request.return_value = {'nodes': [], 'floating_nodes': [{'id': 42, 'focused': True}]}
        sway.focused_workspace.return_value = {'name': 'test'}
        orbit.require_focus(sway, 'test', 42)

    def test_focus_guard_rejects_other_window_or_workspace(self):
        for node_id, workspace in [(43, 'test'), (42, 'other')]:
            with self.subTest(node_id=node_id, workspace=workspace):
                sway = Mock()
                sway.request.return_value = {'nodes': [{'id': node_id, 'focused': True}]}
                sway.focused_workspace.return_value = {'name': workspace}
                with self.assertRaisesRegex(RuntimeError, 'Focus left'):
                    orbit.require_focus(sway, 'test', 42)

    def test_cpu_stat_command_name_can_contain_parenthesis(self):
        # Fields start at process state; utime/stime are offsets 11 and 12.
        fields = ['S'] + ['0'] * 10 + ['250', '75'] + ['0'] * 10
        with patch.object(Path, 'read_text', return_value='123 (name) with spaces) ' + ' '.join(fields)), \
                patch.object(orbit.os, 'sysconf', return_value=100):
            self.assertEqual(orbit.cpu(123), [2.5, 0.75])

    def test_thread_snapshot_preserves_names_and_skips_exited_threads(self):
        fields = ['S'] + ['0'] * 10 + ['250', '75'] + ['0'] * 10
        for vanished in (FileNotFoundError, ProcessLookupError):
            with self.subTest(vanished=vanished):
                with patch.object(Path, 'glob', return_value=[Path('/proc/123/task/123'), Path('/proc/123/task/124')]), \
                        patch.object(Path, 'read_text', side_effect=[
                            '123 (worker) name) ' + ' '.join(fields), 'worker) name\n', vanished(),
                        ]), patch.object(orbit.os, 'sysconf', return_value=100):
                    self.assertEqual(orbit.thread_cpu(123), {
                        '123': {'name': 'worker) name', 'user_seconds': 2.5, 'system_seconds': 0.75},
                    })


if __name__ == '__main__':
    unittest.main()
