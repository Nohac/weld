"""Own one explicit ADB reverse mapping without touching the shared server.

The caller defers shutdown signals through finally cleanup. This avoids ADB
37's stale tcp:0 authorization entry while preserving unrelated mappings.
"""
import secrets
import subprocess


def run(command):
    return subprocess.run(command, check=True, timeout=10, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.strip()


def reverse_mappings(adb):
    return {parts[-2]: parts[-1] for line in run([*adb, "reverse", "--list"]).splitlines()
            if len(parts := line.split()) >= 3}


def create_reverse(adb, host_port, on_created=lambda remote, port: None):
    existing = reverse_mappings(adb)
    if "tcp:0" in existing:
        print("Warning: pre-existing tcp:0 mapping left untouched.", flush=True)
    used = set(existing)
    for _ in range(5):
        # Above Pico's 32768-60999 ephemeral range. The actual bind remains
        # authoritative: other device listeners may occupy these ports too.
        port = 61000 + secrets.randbelow(4536)
        remote, local = f"tcp:{port}", f"tcp:{host_port}"
        if remote in used:
            continue
        used.add(remote)
        try:
            run([*adb, "reverse", "--no-rebind", remote, local])
        except subprocess.CalledProcessError:
            continue
        on_created(remote, port)  # retain ownership even if verification fails
        if reverse_mappings(adb).get(remote) != local:
            raise RuntimeError(f"Could not confirm reverse mapping {remote} to {local}")
        return remote, port
    raise RuntimeError("could not allocate an explicit device reverse port")


def remove_reverse(adb, remote, host_port):
    current = reverse_mappings(adb).get(remote)
    if current is None:
        return
    if current != f"tcp:{host_port}":
        raise RuntimeError(f"mapping {remote} changed ownership; leaving it untouched")
    run([*adb, "reverse", "--remove", remote])


class ReverseTunnel:
    def __init__(self, adb):
        self.adb = tuple(adb)
        self.remote = None
        self.host_port = None

    def open(self, host_port):
        if self.remote is not None:
            raise RuntimeError("reverse tunnel is already owned")
        self.host_port = host_port
        def remember(remote, port):
            self.remote = remote
        _, port = create_reverse(self.adb, host_port, remember)
        return port

    def close(self):
        if self.remote is not None:
            remove_reverse(self.adb, self.remote, self.host_port)
            self.remote = None
