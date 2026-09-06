"""Own policy/rollback tests; never move interfaces or exercise systemd itself."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import stat
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import lifecycle
import main
from policy import Refused, dns_servers, interface_name, validate_namespace, validate_profile, validate_routes, verify_device


def safe_profile():
    return {"uuid": "original", "interface": "usb0", "bound_interface": "usb0",
            "filename": "/etc/NetworkManager/system-connections/tether.nmconnection",
            "unsaved": False, "flags": 0, "compatible_profiles": ["original"],
            "routing_settings": {name: "yes" for name in lifecycle.ROUTING_SETTINGS}}


def physical():
    return {"device_path": "/sys/devices/usb/device", "device_inode": 123,
            "device_event": "binding", "ifindex": 7, "namespace_ifindex": 2}


class PolicyTests(unittest.TestCase):
    def test_profile_must_remain_unambiguous_persistent_and_safe(self):
        validate_profile(safe_profile())
        changes = {"unsaved": True, "flags": 2, "filename": "/run/NetworkManager/temporary",
                   "bound_interface": "", "compatible_profiles": ["original", "another"],
                   "routing_settings": {name: "no" for name in lifecycle.ROUTING_SETTINGS}}
        for key, value in changes.items():
            with self.subTest(key=key), self.assertRaises(Refused):
                validate_profile(safe_profile() | {key: value})

    def test_defaults_and_nameservers_cannot_depend_on_tether(self):
        defaults = {"4": [{"dev": "wifi0"}], "6": []}
        rules = {"4": [{"priority": 32766, "table": "main"}], "6": []}
        dns = [("192.168.1.1", {"dev": "wifi0"})]
        validate_routes("wifi0", "usb0", defaults, rules, dns)
        with self.assertRaises(Refused):
            validate_routes("wifi0", "usb0", defaults | {"6": [{"dev": "usb0"}]}, rules, dns)
        with self.assertRaises(Refused):
            validate_routes("wifi0", "usb0", defaults, rules, [("10.0.0.1", {"dev": "usb0"})])
        with self.assertRaises(Refused):
            validate_routes("wifi0", "usb0", defaults, rules, [("127.0.0.53", {"dev": "wifi0"})])
        with self.assertRaises(Refused):
            validate_routes("wifi0", "usb0", defaults, rules | {"6": [{"priority": 100, "table": 100}]}, dns)

    def test_dns_and_interface_values_are_not_configuration_injection(self):
        self.assertEqual(dns_servers("10.0.0.1 10.0.0.1 1.1.1.1"), ["10.0.0.1", "1.1.1.1"])
        for value in ("", "127.0.0.1", "0.0.0.0", "224.1.2.3", "10.0.0.1\noptions debug"):
            with self.subTest(value=value), self.assertRaises((Refused, ValueError)):
                dns_servers(value)
        for value in ("lo", "--help", "a/b", "a;reboot", "a" * 16):
            with self.subTest(value=value), self.assertRaises(Refused):
                interface_name(value)

    def test_namespace_ifindex_can_change_but_physical_device_cannot(self):
        original = physical()
        verify_device(original, original | {"ifindex": 19})
        verify_device(original, original | {"ifindex": 2, "device_inode": 999}, inside=True)
        with self.assertRaises(Refused):
            verify_device(original, original | {"device_inode": 999})
        with self.assertRaises(Refused):
            verify_device(original, original | {"ifindex": 3}, inside=True)
        with self.assertRaises(Refused):
            verify_device(original, original | {"device_event": "replacement", "ifindex": 2}, inside=True)

    def test_empty_namespace_is_required_before_delete(self):
        validate_namespace(["lo"], [])
        for names, holders in ((["lo", "usb0"], []), (["lo"], ["123"])):
            with self.assertRaises(Refused):
                validate_namespace(names, holders)


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.state = {"config": {"client": "usb0", "host": "wifi0"},
                      "snapshot": {"profile": safe_profile(), "device": physical()},
                      "namespace": "test", "namespace_inode": 99,
                      "namespace_requested": True, "released": True, "move_requested": True,
                      "dhcp_requested": True, "dhcp_unit": "test-dhcp.service", "mountpoints": {}}
        self.calls = []
        self.links = {"lo", "usb0"}
        self.holders = ""
        self.active = "inactive"
        self.profile = safe_profile()
        self.inside = physical() | {"ifindex": 2}
        self.physical_error = None

        def command(program, *args, **kwargs):
            self.calls.append((program, *args))
            if args[:2] == ("netns", "pids"):
                return subprocess.CompletedProcess([], 0, self.holders, "")
            if program == "systemctl" and args[0] == "show":
                return subprocess.CompletedProcess([], 0, self.active, "")
            if "netns" in args and args[-1] == "1":
                self.links.remove("usb0")
            if program == "nmcli" and "GENERAL.CON-UUID" in args:
                return subprocess.CompletedProcess([], 0, "original\n", "")
            return subprocess.CompletedProcess([], 0, "", "")

        mocks = {
            "command": command, "namespace_inode": lambda _: 99,
            "namespace_links": lambda _: set(self.links),
            "profile": lambda *_: self.profile,
            "inspect_inside": lambda *_: self.inside,
            "device": lambda _: physical() | {"ifindex": 27},
            "route_check": lambda *_: None, "write_state": lambda *_: None,
        }
        for name, replacement in mocks.items():
            p = patch.object(lifecycle, name, replacement)
            p.start()
            self.addCleanup(p.stop)
        p = patch.object(lifecycle, "ensure_physical_instance")
        self.physical_check = p.start()
        self.addCleanup(p.stop)
        p = patch.object(Path, "exists", return_value=True)
        p.start()
        self.addCleanup(p.stop)

    def restore(self):
        lifecycle.restore(Path("/owned/run"), self.state)

    def test_stop_lease_then_move_then_restore_original_profile_then_delete(self):
        self.restore()
        stop = self.calls.index(("systemctl", "stop", "test-dhcp.service"))
        move = self.calls.index(("ip", "-n", "test", "link", "set", "dev", "usb0", "netns", "1"))
        up = self.calls.index(("nmcli", "--wait", "20", "connection", "up", "uuid", "original", "ifname", "usb0"))
        delete = self.calls.index(("ip", "netns", "delete", "test"))
        self.assertLess(stop, move)
        self.assertLess(move, up)
        self.assertLess(up, delete)
        self.assertFalse(self.state["released"])

    def test_changed_profile_never_returns_interface_to_nm(self):
        self.profile = safe_profile() | {"uuid": "changed"}
        with self.assertRaises(Refused):
            self.restore()
        self.assertFalse(any(call[0] == "nmcli" or call[-1] == "1" for call in self.calls))

    def test_live_lease_or_namespace_holder_prevents_restore(self):
        for active, holders in (("deactivating", ""), ("inactive", "321")):
            self.active, self.holders = active, holders
            with self.subTest(active=active), self.assertRaises(Refused):
                self.restore()
        self.physical_check.assert_not_called()

    def test_replaced_physical_device_or_extra_interface_prevents_restore(self):
        self.physical_check.side_effect = Refused("unplugged")
        with self.assertRaises(Refused):
            self.restore()
        self.physical_check.side_effect = None
        self.links.add("surprise0")
        with self.assertRaises(Refused):
            self.restore()
        self.assertFalse(any(call[0] == "nmcli" for call in self.calls))

    def test_move_completed_without_ifindex_checkpoint_is_recoverable(self):
        del self.state["snapshot"]["device"]["namespace_ifindex"]
        self.restore()
        self.assertFalse(self.state["released"])

    def test_move_back_completed_before_checkpoint_is_recoverable(self):
        self.links.remove("usb0")
        self.restore()
        self.assertFalse(any(call[-1] == "1" for call in self.calls))
        self.assertFalse(self.state["released"])

    def test_incomplete_namespace_ownership_does_not_guess(self):
        del self.state["namespace_inode"]
        with self.assertRaises(Refused):
            self.restore()

    def test_creation_intent_recovers_only_an_empty_unreleased_namespace(self):
        del self.state["namespace_inode"]
        self.state.update(released=False, move_requested=False)
        self.links = {"lo"}
        self.restore()
        self.assertIn(("ip", "netns", "delete", "test"), self.calls)
        self.physical_check.assert_not_called()

    def test_only_recorded_mountpoint_inode_is_removed(self):
        self.state["mountpoints"] = {"/run/dhcpcd": 123}
        metadata = SimpleNamespace(st_mode=stat.S_IFDIR | 0o700, st_ino=123)
        with patch.object(Path, "lstat", return_value=metadata), patch.object(Path, "rmdir") as remove:
            self.restore()
        remove.assert_called_once()

    def test_unknown_mountpoint_is_preserved_and_cleanup_converges(self):
        self.state.update(closed=False, mountpoints={"/run/dhcpcd": None})
        metadata = SimpleNamespace(st_mode=stat.S_IFDIR | 0o700, st_ino=123)
        with patch.object(lifecycle, "load", return_value=(Path("/owned/run"), self.state)), \
             patch.object(lifecycle, "ensure_host"), \
             patch.object(lifecycle, "device_lock", return_value=contextlib.nullcontext()), \
             patch.object(Path, "lstat", return_value=metadata), patch.object(Path, "rmdir") as remove, \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(lifecycle.cleanup("test"), 0)
            self.assertTrue(self.state["closed"])
            self.assertEqual(lifecycle.cleanup("test"), 0)
        remove.assert_not_called()
        self.assertIn("/run/dhcpcd", self.state["cleanup_notes"][0])


class CleanupStateTests(unittest.TestCase):
    def setUp(self):
        self.state = {"closed": False, "config": {"client": "usb0", "host": "wifi0"}}
        self.directory = Path("/owned/run")
        for name, replacement in (("load", lambda _: (self.directory, self.state)),
                                  ("ensure_host", lambda _: None),
                                  ("device_lock", lambda *args, **kwargs: contextlib.nullcontext()),
                                  ("write_state", lambda *_: None),
                                  ("route_check", lambda *_: None)):
            p = patch.object(lifecycle, name, replacement)
            p.start()
            self.addCleanup(p.stop)
        for stream in ("stdout", "stderr"):
            p = patch.object(sys, stream, io.StringIO())
            p.start()
            self.addCleanup(p.stop)

    def test_lock_refusal_never_writes_stale_state(self):
        with patch.object(lifecycle, "device_lock", side_effect=Refused("busy")), \
             patch.object(lifecycle, "write_state") as write:
            self.assertEqual(lifecycle.cleanup("test"), 1)
        write.assert_not_called()
        self.assertNotIn("recovery_error", self.state)

    def test_reload_under_lock_observes_another_completed_cleanup(self):
        stale = self.state | {"closed": False}
        current = self.state | {"closed": True}
        with patch.object(lifecycle, "load", side_effect=[(self.directory, stale), (self.directory, current)]), \
             patch.object(lifecycle, "restore") as restore:
            self.assertEqual(lifecycle.cleanup("test"), 0)
        restore.assert_not_called()

    def test_restoration_error_remains_retryable(self):
        with patch.object(lifecycle, "restore", side_effect=Refused("device replaced")):
            self.assertEqual(lifecycle.cleanup("test"), 1)
        self.assertFalse(self.state["closed"])
        self.assertEqual(self.state["recovery_error"], "device replaced")

    def test_dns_verification_failure_does_not_repeat_restoration(self):
        with patch.object(lifecycle, "restore") as restore:
            with patch.object(lifecycle, "route_check", side_effect=Refused("DNS unavailable")):
                self.assertEqual(lifecycle.cleanup("test"), 2)
            self.assertTrue(self.state["closed"])
            self.assertIn("verification_error", self.state)
            self.assertEqual(lifecycle.cleanup("test"), 0)
        restore.assert_called_once()
        self.assertNotIn("verification_error", self.state)

    def test_mountpoint_creation_records_intent_then_inode(self):
        checkpoints = []
        state = {"mountpoints": {}}
        metadata = SimpleNamespace(st_uid=0, st_mode=stat.S_IFDIR | 0o700, st_ino=123)
        with patch.object(Path, "exists", side_effect=[False, True]), \
             patch.object(Path, "mkdir") as create, patch.object(Path, "stat", return_value=metadata), \
             patch.object(Path, "lstat", return_value=metadata), \
             patch.object(lifecycle, "write_state", side_effect=lambda _, value: checkpoints.append(json.loads(json.dumps(value)))):
            lifecycle.create_mountpoints(self.directory, state)
        create.assert_called_once_with(mode=0o700)
        self.assertIsNone(checkpoints[0]["mountpoints"]["/run/dhcpcd"])
        self.assertEqual(checkpoints[1]["mountpoints"]["/run/dhcpcd"], 123)
        self.assertNotIn("/var/lib/dhcpcd", state["mountpoints"])


class DhcpHookTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        (self.directory / "dhcp").mkdir()
        self.resolver = self.directory / "dhcp" / "resolv.conf"
        self.resolver.write_text("old contents\n")
        self.inode = self.resolver.stat().st_ino
        self.state = {"namespace_inode": 99, "config": {"client": "usb0"}, "resolver_inode": self.inode}
        for name, replacement in (("load", lambda _: (self.directory, self.state)),
                                  ("namespace_inode", lambda _: 99), ("require_root", lambda: None)):
            p = patch.object(lifecycle, name, replacement)
            p.start()
            self.addCleanup(p.stop)

    def hook(self, reason, *, servers="10.0.0.1", inode=None):
        metadata = SimpleNamespace(st_uid=0, st_mode=stat.S_IFREG | 0o644,
                                   st_ino=self.inode if inode is None else inode)
        with patch.dict(os.environ, {"interface": "usb0", "reason": reason, "new_domain_name_servers": servers}), \
             patch.object(os, "fstat", return_value=metadata):
            lifecycle.dhcp_hook("test")

    def status(self):
        return json.loads((self.directory / "dhcp" / "lease.json").read_text())

    def test_ack_updates_same_resolver_inode_and_publishes_ready(self):
        self.hook("BOUND")
        self.assertEqual(self.resolver.stat().st_ino, self.inode)
        self.assertEqual(self.resolver.read_text(), "nameserver 10.0.0.1\n")
        self.assertEqual(self.status(), {"ready": True, "acquired": True, "reason": "BOUND"})
        self.hook("RENEW", servers="1.1.1.1")
        self.assertEqual(self.resolver.read_text(), "nameserver 1.1.1.1\n")

    def test_replaced_resolver_is_not_written(self):
        self.hook("BOUND", inode=self.inode + 1)
        self.assertIn("replaced", self.status()["error"])
        self.assertEqual(self.resolver.read_text(), "old contents\n")

    def test_ignored_reason_and_lease_loss(self):
        self.hook("PREINIT")
        self.assertFalse((self.directory / "dhcp" / "lease.json").exists())
        self.hook("BOUND")
        self.hook("NOCARRIER")
        self.assertFalse(self.status()["ready"])
        self.assertIn("lease lost", self.status()["error"])
        failed = self.status()
        self.assertTrue(failed["acquired"])
        for reason in ("NOCARRIER", "CARRIER", "BOUND"):
            self.hook(reason)
            self.assertEqual(self.status(), failed)

    def test_initial_carrier_wait_then_acquisition(self):
        for _ in range(2):
            self.hook("NOCARRIER", servers="")
            self.assertEqual(self.status(), {"ready": False, "acquired": False, "reason": "NOCARRIER"})
        self.hook("CARRIER")
        self.assertFalse(self.status()["ready"])
        self.assertNotIn("error", self.status())
        self.assertEqual(self.resolver.read_text(), "old contents\n")
        self.hook("BOUND")
        self.assertTrue(self.status()["ready"])
        self.assertTrue(self.status()["acquired"])
        self.assertEqual(self.resolver.read_text(), "nameserver 10.0.0.1\n")

    def test_terminal_event_before_first_lease_is_still_fatal(self):
        self.hook("STOP")
        self.assertFalse(self.status()["acquired"])
        self.assertIn("error", self.status())

    def test_initial_carrier_wait_cannot_hide_failed_lease_validation(self):
        self.hook("BOUND", servers="")
        failed = self.status()
        self.assertFalse(failed["acquired"])
        self.assertIn("nameservers", failed["error"])
        for reason in ("NOCARRIER", "CARRIER", "BOUND"):
            self.hook(reason)
            self.assertEqual(self.status(), failed)

    def test_missing_dns_is_reported_not_silently_accepted(self):
        self.hook("BOUND", servers="")
        self.assertIn("nameservers", self.status()["error"])


class UserBoundaryTests(unittest.TestCase):
    def test_runtime_environment_is_loaded_only_after_privilege_drop(self):
        state = {"config": {"uid": 1000, "gid": 1000}, "python": "/trusted/python",
                 "entry": "/repo/main.py", "manifest": "/private/run.json"}
        with patch.object(lifecycle, "executable", return_value="/trusted/setpriv"):
            args = lifecycle.user_command(state, "destination")
        self.assertEqual(args[:4], ["/trusted/setpriv", "--reuid=1000", "--regid=1000", "--init-groups"])
        self.assertIn("--no-new-privs", args)
        self.assertNotIn("LD_LIBRARY_PATH", " ".join(args))
        self.assertEqual(args[-3:], ["_exec", "/private/run.json", "destination"])

    def test_private_manifest_rejects_readable_file_and_symlink(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "run.json"
            main.save_private(path, {"test": True})
            self.assertEqual(lifecycle.read_private(path, os.getuid()), {"test": True})
            path.chmod(0o644)
            with self.assertRaises(Refused):
                lifecycle.read_private(path, os.getuid())
            link = Path(temporary) / "link.json"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                lifecycle.read_private(link, os.getuid())

    def test_dry_run_never_builds_or_elevates(self):
        with patch.object(sys, "argv", ["launcher", "--host", "wifi0", "--client", "usb0", "--dhcpcd", "/trusted/dhcpcd", "--dry-run"]), \
             patch.object(lifecycle, "preflight", return_value={}), \
             patch.object(os, "geteuid", return_value=1000), \
             patch.object(subprocess, "run") as run, patch.object(subprocess, "call") as call, \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main.launch(), 0)
        run.assert_not_called()
        call.assert_not_called()

    def test_peer_argv_keeps_admission_and_runtime_environment(self):
        config = {"uid": 1000, "id": "a" * 32, "startup_seconds": 120, "codec": "av1",
                  "command": ["foot"], "repository": "/repo", "runtime": {
                      "binary": "/repo/target/debug/weldwm", "environment": {"LD_LIBRARY_PATH": "/user/libs"}}}
        with patch.object(lifecycle, "read_private", return_value=config), \
             patch.object(os, "geteuid", return_value=1000), patch.object(os, "umask"), \
             patch.object(os, "open", return_value=42), patch.object(os, "dup2"), \
             patch.object(os, "close"), patch.object(os, "chdir"), patch.object(os, "execve") as execute:
            main.execute_user("/private/run.json", "source")
            _, args, environment = execute.call_args.args
            self.assertIn("--hoist-iroh-expect-peer", args)
            self.assertEqual(args[-2:], ["--", "foot"])
            self.assertEqual(environment["LD_LIBRARY_PATH"], "/user/libs")
            self.assertEqual(environment["WINIT_UNIX_BACKEND"], "wayland")
            self.assertIn("weld_media_diag=debug", environment["RUST_LOG"])
            config["runtime"]["environment"]["RUST_LOG"] = "error"
            main.execute_user("/private/run.json", "destination")
            self.assertIn("--hoist-iroh-publish-identity", execute.call_args.args[1])
            self.assertEqual(execute.call_args.args[2]["RUST_LOG"], "error")
            with patch.object(os, "geteuid", return_value=0), self.assertRaises(Refused):
                main.execute_user("/private/run.json", "source")


if __name__ == "__main__":
    unittest.main()
