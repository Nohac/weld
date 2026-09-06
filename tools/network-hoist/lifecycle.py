"""Privileged lifecycle for one developer-owned network test, not an installed sudo API.

The supervisor stays in the host mount/network namespaces. Only DHCP and the
receiver get private mount namespaces. Recovery uses root-owned phase intents,
never a user-writable manifest, and never guesses which replacement NIC to use.
"""

import contextlib
from contextvars import ContextVar
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import pwd
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace

from policy import (Refused, dns_servers, interface_name, run_id, seconds,
                    validate_namespace, validate_profile, validate_routes, verify_device)

ROOT = Path("/run/weld-network")
# 5s lock + 15s DHCP stop + 5s NM readiness + 25s activation leaves
# 20s for checks here, and another 20s before systemd's 90s stop ceiling.
CLEANUP_SECONDS = 70
TOOL_PATH = "/run/current-system/sw/bin:/usr/sbin:/usr/bin:/sbin:/bin"
ROOT_ENV = {"PATH": TOOL_PATH, "LANG": "C", "LC_ALL": "C"}
COMMAND_DEADLINE = ContextVar("command_deadline", default=None)
NM = "org.freedesktop.NetworkManager"
SETTINGS = NM + ".Settings.Connection"
ROUTING_SETTINGS = ("ipv4.never-default", "ipv6.never-default",
                    "ipv4.ignore-auto-dns", "ipv6.ignore-auto-dns")


def executable(name):
    path = Path(name) if "/" in str(name) else Path(shutil.which(name, path=TOOL_PATH) or "/missing-tool")
    path = path.resolve(strict=True)
    metadata = path.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
        raise Refused(f"privileged tool is not a root-owned, non-writable executable: {path}")
    if not os.access(path, os.X_OK):
        raise Refused(f"tool is not executable: {path}")
    return str(path)


def command(program, *args, timeout=15, check=True):
    deadline = COMMAND_DEADLINE.get()
    if deadline is not None:
        timeout = min(timeout, deadline - time.monotonic())
        if timeout <= 0:
            raise Refused("recovery deadline reached; retry the printed recovery command")
    result = subprocess.run([executable(program), *map(str, args)], env=ROOT_ENV,
                            text=True, capture_output=True, timeout=timeout)
    if check and result.returncode:
        raise Refused(f"{Path(program).name} failed: {result.stderr.strip()[-1500:]}")
    return result


def query(program, *args):
    return json.loads(command(program, *args).stdout)


def namespace_inode(path):
    return Path(path).stat().st_ino


def host_context():
    if Path("/proc/1/comm").read_text().strip() != "systemd":
        raise Refused("run from the systemd host, not a container or process-isolated sandbox")
    result = {kind: namespace_inode(f"/proc/self/ns/{kind}") for kind in ("net", "mnt")}
    try:
        initial = {kind: namespace_inode(f"/proc/1/ns/{kind}") for kind in result}
    except PermissionError:
        if os.geteuid() == 0:
            raise
        # ptrace restrictions commonly hide PID 1's namespace links from users.
        # Root prepare and supervisor repeat this check before any interface move.
        return result | {"boot": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}
    if result != initial:
        raise Refused("launcher/supervisor must use the host network AND mount namespaces")
    result["boot"] = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    return result


def device(interface):
    interface_name(interface)
    path = Path("/sys/class/net") / interface
    physical = (path / "device").resolve(strict=True)
    info = query("ip", "-j", "link", "show", "dev", interface)[0]
    if info.get("linkinfo") or info.get("master") or info.get("link_type") != "ether":
        raise Refused("client must be a standalone physical Ethernet/USB interface")
    if (path / "wireless").exists():
        raise Refused("moving a wireless client interface is not supported")
    return {"ifindex": info["ifindex"], "device_path": str(physical),
            "device_inode": physical.stat().st_ino,
            "device_event": hashlib.sha256((physical / "uevent").read_bytes()).hexdigest()}


def nm_property(path, interface, name):
    return query("busctl", "--system", "--json=short", "get-property", NM, path, interface, name)["data"]


def profile(interface, identifier=None):
    identifier = identifier or command("nmcli", "-g", "GENERAL.CON-UUID", "device", "show", interface).stdout.strip()
    path = query("busctl", "--system", "--json=short", "call", NM,
                 "/org/freedesktop/NetworkManager/Settings", NM + ".Settings",
                 "GetConnectionByUuid", "s", identifier)["data"][0]
    values = command("nmcli", "-g", ",".join(("connection.interface-name", *ROUTING_SETTINGS)),
                     "connection", "show", "uuid", identifier).stdout.splitlines()
    if len(values) != 5:
        raise Refused("could not read stable tether profile settings")
    compatible = []
    for row in command("nmcli", "-t", "-f", "UUID,TYPE", "connection", "show").stdout.splitlines():
        candidate, kind = row.split(":", 1)
        if kind in ("ethernet", "802-3-ethernet"):
            binding = command("nmcli", "-g", "connection.interface-name", "connection", "show", "uuid", candidate).stdout.strip()
            if binding in ("", "--", interface):
                compatible.append(candidate)
    result = {"uuid": identifier, "interface": interface, "bound_interface": values[0],
              "routing_settings": dict(zip(ROUTING_SETTINGS, values[1:])),
              "filename": nm_property(path, SETTINGS, "Filename"),
              "unsaved": nm_property(path, SETTINGS, "Unsaved"),
              "flags": nm_property(path, SETTINGS, "Flags"),
              "compatible_profiles": sorted(compatible)}
    validate_profile(result)
    if os.geteuid() == 0:
        with os.fdopen(os.open(result["filename"], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as keyfile:
            metadata = os.fstat(keyfile.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o077:
                raise Refused("persistent tether keyfile must be a private root-owned regular file")
    return result


def route_check(host, client):
    defaults = {family: query("ip", "-" + family, "-j", "route", "show", "default") for family in ("4", "6")}
    rules = {family: query("ip", "-" + family, "-j", "rule", "show") for family in ("4", "6")}
    resolvers = []
    for line in Path("/etc/resolv.conf").read_text().splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[0] == "nameserver":
            address = ipaddress.ip_address(fields[1])
            routes = query("ip", f"-{address.version}", "-j", "route", "get", str(address))
            if not routes:
                raise Refused("nameserver has no host route")
            resolvers.append((str(address), routes[0]))
    validate_routes(host, client, defaults, rules, resolvers)
    return {"defaults": defaults, "nameservers": [server for server, _ in resolvers]}


def preflight(host, client, dhcpcd):
    interface_name(host)
    interface_name(client)
    context = host_context()
    for tool in ("ip", "nmcli", "busctl", "systemd-run", "systemctl", "setpriv", "unshare", "mount"):
        executable(tool)
    dhcpcd = executable(dhcpcd)
    version = command(dhcpcd, "--version").stdout
    if not version.startswith("dhcpcd 10.") or "PRIVSEP" in version:
        raise Refused("this first launcher requires dhcpcd 10.x without PRIVSEP (validated with 10.3.2)")
    if command("nmcli", "-g", "GENERAL.NM-MANAGED", "device", "show", client).stdout.strip() != "yes":
        raise Refused("client interface must initially be managed by NetworkManager")
    return {"context": context, "profile": profile(client), "device": device(client),
            "routes": route_check(host, client)}


def read_private(path, owner):
    parent = path.parent.stat()
    if parent.st_uid != owner or stat.S_IMODE(parent.st_mode) != 0o700:
        raise Refused("manifest directory must be private and owned by the expected user")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "r") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != owner or stat.S_IMODE(metadata.st_mode) != 0o600:
            raise Refused("manifest must be a private regular file owned by the expected user")
        raw = source.read(256 * 1024 + 1)
        if len(raw) > 256 * 1024:
            raise Refused("manifest is too large")
        return json.loads(raw)


def write_state(directory, state):
    with tempfile.NamedTemporaryFile(mode="w", dir=directory, delete=False) as output:
        temporary = Path(output.name)
        json.dump(state, output)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, directory / "state.json")


def unit_name(identifier):
    return "weld-network-" + run_id(identifier) + ".service"


def root_directory(identifier):
    path = ROOT / run_id(identifier)
    for directory in (ROOT, path):
        metadata = directory.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != 0 or stat.S_IMODE(metadata.st_mode) != 0o700:
            raise Refused("recovery state directory is not private and root-owned")
    return path


def load(identifier):
    directory = root_directory(identifier)
    return directory, read_private(directory / "state.json", 0)


@contextlib.contextmanager
def device_lock(client, wait_seconds=0):
    fd = os.open(ROOT / (interface_name(client) + ".lock"), os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or stat.S_IMODE(metadata.st_mode) != 0o600:
            raise Refused("device lock is not a private root-owned regular file")
        deadline = min(time.monotonic() + wait_seconds, COMMAND_DEADLINE.get() or float("inf"))
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError as error:
                if time.monotonic() >= deadline:
                    raise Refused("another test or recovery currently owns this client interface") from error
                time.sleep(0.1)
        yield
    finally:
        os.close(fd)


@contextlib.contextmanager
def record_interrupts(ignore=False):
    """Do not raise through Popen.wait and accidentally kill the supervised child."""
    status = SimpleNamespace(requested=False)

    def received(_signal, _frame):
        status.requested = True

    previous = {number: signal.signal(number, signal.SIG_IGN if ignore else received)
                for number in (signal.SIGINT, signal.SIGQUIT)}
    try:
        yield status
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)


def report(message, *, error=False):
    """Terminal output is not part of successful restoration's contract."""
    stream = sys.stderr if error else sys.stdout
    try:
        print(message, file=stream, flush=True)
    except BrokenPipeError:
        # Keep the existing stream's buffered final flush from failing with 120.
        descriptor = os.open(os.devnull, os.O_WRONLY)
        try:
            os.dup2(descriptor, stream.fileno())
        finally:
            os.close(descriptor)


def prepare(manifest):
    with record_interrupts() as interruption:
        try:
            return prepare_run(manifest, interruption)
        except (Refused, OSError, ValueError, subprocess.SubprocessError):
            if interruption.requested:
                report("Network test cancelled before supervision started.")
                return 130
            raise


def prepare_run(manifest, interruption):
    owner = int(os.environ.get("SUDO_UID", "0"))
    if owner == 0:
        raise Refused("start via the normal-user launcher and sudo")
    manifest = Path(manifest).absolute()
    config = read_private(manifest, owner)
    if config["uid"] != owner or config["gid"] != pwd.getpwuid(owner).pw_gid:
        raise Refused("manifest identity does not match the invoking user")
    identifier = run_id(config["id"])
    seconds(config["seconds"])
    seconds(config["startup_seconds"])
    fresh = preflight(config["host"], config["client"], config["dhcpcd"])
    if fresh["context"] != config["snapshot"]["context"] or fresh["profile"] != config["snapshot"]["profile"]:
        raise Refused("host namespace or tether profile changed during the build; rerun preflight")
    verify_device(config["snapshot"]["device"], fresh["device"])
    ROOT.mkdir(mode=0o700, exist_ok=True)
    metadata = ROOT.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != 0 or stat.S_IMODE(metadata.st_mode) != 0o700:
        raise Refused("recovery root is not a private root-owned directory")
    directory = ROOT / identifier
    directory.mkdir(mode=0o700)
    root_directory(identifier)
    helper_hashes = {}
    for filename in ("main.py", "lifecycle.py", "policy.py"):
        contents = (Path(__file__).parent / filename).read_text()
        if filename == "main.py":
            contents = "#!" + executable(sys.executable) + " -I\n" + contents.split("\n", 1)[1]
        target = directory / filename
        target.write_text(contents)
        target.chmod(0o500)
        helper_hashes[filename] = hashlib.sha256(target.read_bytes()).hexdigest()
    (directory / "dhcp").mkdir(mode=0o700)
    (directory / "dhcp" / "resolv.conf").write_text("")
    # Bind-mounted at /etc/resolv.conf for the user receiver: source traversal
    # remains root-only, but the bound file itself must be readable after UID drop.
    (directory / "dhcp" / "resolv.conf").chmod(0o644)
    (directory / "dhcpcd.conf").write_text("option domain_name_servers\nrequire dhcp_server_identifier\n")
    state = {"config": config, "snapshot": fresh, "manifest": str(manifest),
             "entry": str(Path(__file__).parent / "main.py"), "python": executable(sys.executable),
             "namespace": "weld-" + identifier, "dhcp_unit": "weld-network-dhcp-" + identifier + ".service",
             "namespace_requested": False, "released": False, "move_requested": False,
             "resolver_inode": (directory / "dhcp" / "resolv.conf").stat().st_ino,
             "helper_sha256": helper_hashes,
             "mountpoints": {}, "closed": False}
    write_state(directory, state)
    helper = directory / "main.py"
    total = config["startup_seconds"] + config["seconds"] + 15
    arguments = [executable("systemd-run"), "--unit=" + unit_name(identifier),
        "--wait", "--pipe", "--collect", "--service-type=exec",
        "--property=KillMode=control-group", "--property=TimeoutStopSec=90",
        "--property=LimitCORE=0", "--property=PrivateMounts=no",
        "--property=UnsetEnvironment=LD_LIBRARY_PATH LD_PRELOAD PYTHONPATH PYTHONHOME",
        "--property=RuntimeMaxSec=" + str(total),
        "--property=ExecStopPost=" + str(helper) + " _cleanup " + identifier,
        str(helper), "_service", identifier]
    return run_unit(arguments, identifier, interruption)


def run_unit(arguments, identifier, interruption):
    if interruption.requested:
        report("Network test cancelled before starting the service.")
        return 130
    # Only the already-privileged systemd-run waiter is isolated. sudo stays in
    # the foreground, where it can prompt and forward signals to _prepare.
    process = subprocess.Popen(arguments, env=ROOT_ENV, start_new_session=True)
    while True:
        if interruption.requested:
            return cancel_unit(process, identifier)
        try:
            result = process.wait(timeout=0.2)
            return cancel_unit(process, identifier) if interruption.requested else result
        except subprocess.TimeoutExpired:
            pass


def cancel_unit(process, identifier):
    with record_interrupts(ignore=True):
        report("Cancellation requested; stopping this run and restoring the tether. "
               "Allow up to about 90 seconds, longer if the first cleanup did not finish.")
        try:
            directory, _state = load(identifier)
            # Separate from state.json: the running service owns its checkpoints.
            # A unit registered after stop's lookup must not start a new move.
            try:
                descriptor = os.open(directory / "cancelled", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            except FileExistsError:
                pass
            else:
                os.close(descriptor)
            command("systemctl", "stop", unit_name(identifier), timeout=100, check=False)
            process.wait(timeout=100)
            result = cleanup(identifier)
            return 130 if result == 0 else result
        except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
            report(f"Cancellation recovery incomplete: {error}\n"
                   f"Retry: scripts/run-network-hoist --recover {identifier}", error=True)
            return 1


def mark(directory, state, key, value=True):
    state[key] = value
    write_state(directory, state)


def ensure_host(state):
    if host_context() != state["snapshot"]["context"]:
        raise Refused("supervisor/recovery is not in the original host namespaces and boot")


def ensure_physical_instance(state):
    recorded = state["snapshot"]["device"]
    physical = Path(recorded["device_path"])
    if physical.stat().st_ino != recorded["device_inode"]:
        raise Refused("tether was unplugged/replaced; refusing to claim a new device")
    if hashlib.sha256((physical / "uevent").read_bytes()).hexdigest() != recorded["device_event"]:
        raise Refused("physical device binding changed")


def inspect_inside(directory, state):
    result = command("ip", "netns", "exec", state["namespace"], directory / "main.py",
                     "_inspect", state["config"]["client"])
    return json.loads(result.stdout)


def create_mountpoints(directory, state):
    for name in ("/run/dhcpcd", "/var/lib/dhcpcd"):
        path = Path(name)
        if path.exists():
            metadata = path.lstat()
            if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != 0:
                raise Refused("DHCP mountpoint is not a root-owned directory")
            continue
        state["mountpoints"][name] = None
        write_state(directory, state)
        path.mkdir(mode=0o700)
        state["mountpoints"][name] = path.stat().st_ino
        write_state(directory, state)


def user_command(state, role):
    config = state["config"]
    return [executable("setpriv"), "--reuid=" + str(config["uid"]),
            "--regid=" + str(config["gid"]), "--init-groups", "--no-new-privs",
            state["python"], "-I", state["entry"], "_exec", state["manifest"], role]


def service(identifier):
    directory, state = load(identifier)
    ensure_host(state)
    config = state["config"]
    client = config["client"]
    with device_lock(client):
        directory, state = load(identifier)
        if state.get("closed") or (directory / "cancelled").exists():
            return 0
        fresh = preflight(config["host"], client, config["dhcpcd"])
        if fresh["profile"] != state["snapshot"]["profile"]:
            raise Refused("tether profile changed before setup")
        verify_device(state["snapshot"]["device"], fresh["device"])
        if Path("/run/netns", state["namespace"]).exists():
            raise Refused("namespace already exists")
        deadline = time.monotonic() + config["startup_seconds"]
        report("Creating isolated receiver namespace.")
        mark(directory, state, "namespace_requested")
        command("ip", "netns", "add", state["namespace"])
        mark(directory, state, "namespace_inode", namespace_inode("/run/netns/" + state["namespace"]))
        mark(directory, state, "released")
        command("nmcli", "--wait", "10", "device", "set", client, "managed", "no")
        if command("nmcli", "-g", "GENERAL.NM-MANAGED", "device", "show", client).stdout.strip() != "no":
            raise Refused("NetworkManager did not release the client interface")
        mark(directory, state, "move_requested")
        command("ip", "link", "set", "dev", client, "netns", state["namespace"])
        ensure_physical_instance(state)
        moved = inspect_inside(directory, state)
        if (moved["device_path"], moved["device_event"]) != (fresh["device"]["device_path"], fresh["device"]["device_event"]):
            raise Refused("moved device identity did not match")
        state["snapshot"]["device"]["namespace_ifindex"] = moved["ifindex"]
        write_state(directory, state)
        command("ip", "-n", state["namespace"], "link", "set", "lo", "up")
        create_mountpoints(directory, state)
        helper = directory / "main.py"
        report("Acquiring a fresh namespace lease (host DHCP state is isolated).")
        mark(directory, state, "dhcp_requested")
        command("systemd-run", "--unit=" + state["dhcp_unit"], "--service-type=exec", "--collect",
                "--property=BindsTo=" + unit_name(identifier), "--property=After=" + unit_name(identifier),
                "--property=KillMode=control-group", "--property=TimeoutStopSec=10", "--property=LimitCORE=0",
                "--property=RuntimeMaxSec=" + str(config["startup_seconds"] + config["seconds"] + 15),
                "--property=ProtectSystem=strict", "--property=ProtectHostname=yes",
                "--property=TemporaryFileSystem=/run/dhcpcd:mode=0700 /var/lib/dhcpcd:mode=0700",
                "--property=ReadWritePaths=" + str(directory / "dhcp") + " /run/dhcpcd /var/lib/dhcpcd",
                "--property=UnsetEnvironment=LD_LIBRARY_PATH LD_PRELOAD PYTHONPATH PYTHONHOME",
                helper, "_lease", identifier)
        next_unit_check = 0
        while True:
            status = lease_status(directory)
            if time.monotonic() >= deadline:
                raise Refused(f"startup deadline expired waiting for the namespace lease; last hook status: {status}")
            if status.get("error"):
                raise Refused("DHCP hook failed: " + status["error"])
            if status.get("ready"):
                break
            if time.monotonic() >= next_unit_check:
                if not unit_active(state["dhcp_unit"]):
                    raise Refused("DHCP unit stopped before acquiring a lease; inspect its journal")
                next_unit_check = time.monotonic() + 1
            time.sleep(0.2)
        namespace_routes = query("ip", "-n", state["namespace"], "-4", "-j", "route", "show", "default")
        if not namespace_routes or any(route.get("dev") != client for route in namespace_routes):
            raise Refused("receiver did not obtain its own client-interface default route")
        links = query("ip", "-n", state["namespace"], "-j", "link", "show")
        if {link["ifname"] for link in links} != {"lo", client}:
            raise Refused("receiver namespace contains an unexpected interface")
        route_check(config["host"], client)
        counters_before = counters(state)
        source = subprocess.Popen(user_command(state, "source"), env=ROOT_ENV)
        destination = subprocess.Popen([executable("unshare"), "--mount", "--propagation", "private",
                                        str(helper), "_receiver", identifier], env=ROOT_ENV)
        runtime_dir = Path(config["runtime"]["environment"]["XDG_RUNTIME_DIR"])
        sockets = [runtime_dir / f"weld-net-{identifier[:12]}-{role}" for role in ("source", "destination")]
        while not all(path.is_socket() and path.stat().st_uid == config["uid"] for path in sockets):
            if source.poll() is not None or destination.poll() is not None:
                raise Refused(f"Weld exited during startup: source={source.poll()}, destination={destination.poll()}")
            if time.monotonic() >= deadline:
                raise Refused("startup deadline expired waiting for both Weld runtimes")
            time.sleep(0.2)
        report(f"Both Weld instances ready. Test for {config['seconds']} seconds; focus source and press Super+H.")
        end = time.monotonic() + config["seconds"]
        while time.monotonic() < end:
            if source.poll() is not None or destination.poll() is not None:
                report(f"Peer exited: source={source.poll()}, destination={destination.poll()}")
                if any(code not in (None, 0) for code in (source.poll(), destination.poll())):
                    raise Refused("a Weld peer failed; inspect the private source/destination logs")
                break
            ensure_physical_instance(state)
            if lease_status(directory).get("error") or not unit_active(state["dhcp_unit"]):
                raise Refused("receiver lease was lost")
            time.sleep(1)
        after = counters(state)
        report(f"Client-interface byte delta (includes DHCP/N0): RX {after[0] - counters_before[0]}, TX {after[1] - counters_before[1]}")
        report("Run finished; systemd will stop this run's processes before restoring the tether.")
    return 0


def unit_active(name):
    return command("systemctl", "is-active", "--quiet", name, check=False).returncode == 0


def counters(state):
    info = query("ip", "-n", state["namespace"], "-s", "-j", "link", "show", "dev", state["config"]["client"])[0]
    stats = info.get("stats64", info.get("stats", {}))
    return stats["rx"]["bytes"], stats["tx"]["bytes"]


def lease_status(directory):
    path = directory / "dhcp" / "lease.json"
    return json.loads(path.read_text()) if path.exists() else {}


def bind_resolver(directory, state):
    if namespace_inode("/proc/self/ns/mnt") == state["snapshot"]["context"]["mnt"]:
        raise Refused("refusing to bind a resolver in the host mount namespace")
    command("mount", "--bind", directory / "dhcp" / "resolv.conf", "/etc/resolv.conf")
    command("mount", "-o", "remount,bind,ro", "/etc/resolv.conf")


def enter_receiver(identifier):
    directory, state = load(identifier)
    bind_resolver(directory, state)
    args = [executable("ip"), "netns", "exec", state["namespace"], *user_command(state, "destination")]
    os.execve(args[0], args, ROOT_ENV)


def lease_process(identifier, mounted=False):
    directory, state = load(identifier)
    helper = str(directory / "main.py")
    if not mounted:
        args = [executable("unshare"), "--mount", "--propagation", "private", helper, "_lease-mounted", identifier]
    else:
        bind_resolver(directory, state)
        args = [executable("ip"), "netns", "exec", state["namespace"],
                executable(state["config"]["dhcpcd"]), "-B", "-4", "-L",
                "-f", str(directory / "dhcpcd.conf"), "-c", helper,
                "-e", "WELD_NETWORK_DHCP_RUN=" + identifier, state["config"]["client"]]
    os.execve(args[0], args, ROOT_ENV)


def dhcp_hook(identifier):
    require_root()
    directory, state = load(identifier)
    if namespace_inode("/proc/self/ns/net") != state.get("namespace_inode") or os.environ.get("interface") != state["config"]["client"]:
        raise Refused("DHCP hook invoked outside its owned namespace/interface")
    reason = os.environ["reason"]
    success = reason in ("BOUND", "RENEW", "REBIND", "REBOOT")
    failure = reason in ("EXPIRE", "RELEASE", "STOP", "STOPPED", "DEPARTED", "NOCARRIER", "FAIL")
    if not success and not failure:
        return 0
    # dhcpcd serializes this interface's hooks; the supervisor only reads status.
    # Keep fatal errors sticky so a later event cannot hide them between polls.
    previous = lease_status(directory)
    if previous.get("error"):
        return 0
    acquired = previous.get("acquired", False)
    status = {"ready": False, "acquired": acquired, "reason": reason}
    # Moving a device can briefly drop carrier before its first lease. This is
    # startup progress, not lease loss; the supervisor's deadline still applies.
    if reason != "NOCARRIER" or acquired:
        try:
            if failure:
                raise Refused("lease lost: " + reason)
            servers = dns_servers(os.environ.get("new_domain_name_servers", ""))
            path = directory / "dhcp" / "resolv.conf"
            fd = os.open(path, os.O_WRONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, "w") as output:
                metadata = os.fstat(fd)
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_ino != state["resolver_inode"]:
                    raise Refused("namespace resolver was replaced")
                # A rename would strand the bind mount on the old inode. Update this
                # owned inode in place, then publish readiness separately and atomically.
                output.truncate(0)
                output.write("".join("nameserver " + address + "\n" for address in servers))
                output.flush()
            status.update(ready=True, acquired=True)
        except (Refused, OSError, ValueError) as error:
            status["error"] = str(error)
    with tempfile.NamedTemporaryFile(mode="w", dir=directory / "dhcp", delete=False) as output:
        json.dump(status, output)
        temporary = Path(output.name)
    os.replace(temporary, directory / "dhcp" / "lease.json")
    return 0


def namespace_links(state):
    return {link["ifname"] for link in query("ip", "-n", state["namespace"], "-j", "link", "show")}


def cleanup(identifier):
    """Retryable rollback; intents cover termination between a syscall and its checkpoint."""
    directory, state = load(identifier)
    token = COMMAND_DEADLINE.set(time.monotonic() + CLEANUP_SECONDS)
    try:
        with device_lock(state["config"]["client"], wait_seconds=5):
            # Another recovery may have progressed while we waited. Never write a
            # stale snapshot, and never write anything if we failed to acquire ownership.
            directory, state = load(identifier)
            ensure_host(state)
            if state["closed"] and not state.get("verification_error"):
                return 0
            if not state["closed"]:
                try:
                    restore(directory, state)
                except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
                    mark(directory, state, "recovery_error", str(error))
                    raise
                state.pop("recovery_error", None)
                mark(directory, state, "closed")
            try:
                route_check(state["config"]["host"], state["config"]["client"])
            except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
                mark(directory, state, "verification_error", str(error))
                report(f"Tether restored, but host routing/DNS verification FAILED: {error}\n"
                       f"Check host connectivity, then re-verify: scripts/run-network-hoist --recover {identifier}", error=True)
                return 2
            state.pop("verification_error", None)
            write_state(directory, state)
            report("Tether restored; this run's namespace and processes are gone.")
            for note in state.get("cleanup_notes", []):
                report("Cleanup note: " + note)
        return 0
    except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
        report(f"Recovery incomplete: {error}\nRetry: scripts/run-network-hoist --recover {identifier}", error=True)
        return 1
    finally:
        COMMAND_DEADLINE.reset(token)


def wait_for_network_manager(state):
    deadline = min(time.monotonic() + 5, COMMAND_DEADLINE.get() or float("inf"))
    token = COMMAND_DEADLINE.set(deadline)
    try:
        wait_for_managed_device(state, deadline)
    finally:
        COMMAND_DEADLINE.reset(token)


def wait_for_managed_device(state, deadline):
    client = state["config"]["client"]
    original = state["snapshot"]["profile"]
    while True:
        ensure_physical_instance(state)
        verify_device(state["snapshot"]["device"], device(client))
        if profile(client, original["uuid"]) != original:
            raise Refused("original safe tether profile changed while waiting for NetworkManager")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Refused("NetworkManager did not manage the returning tether before the readiness deadline")
        # An unknown device is expected briefly after netns return. Both the
        # setter and query may fail until NM has registered it; neither is proof
        # that it is safe to activate a different device or profile.
        managed = command("nmcli", "--wait", str(int(remaining)), "device", "set", client, "managed", "yes",
                          timeout=remaining, check=False)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Refused("NetworkManager tether readiness deadline reached")
        result = command("nmcli", "-g", "GENERAL.NM-MANAGED", "device", "show", client,
                         timeout=remaining, check=False)
        if managed.returncode == 0 and result.returncode == 0 and result.stdout.strip() == "yes":
            return
        time.sleep(min(0.1, max(0, deadline - time.monotonic())))


def restore(directory, state):
    client = state["config"]["client"]
    namespace = Path("/run/netns") / state["namespace"]
    if state.get("dhcp_requested"):
        command("systemctl", "stop", state["dhcp_unit"], timeout=15, check=False)
        # is-active alone misses activating/deactivating units with live processes.
        activity = command("systemctl", "show", "-p", "ActiveState", "--value", state["dhcp_unit"], check=False).stdout.strip()
        if activity not in ("", "inactive", "failed"):
            raise Refused("DHCP unit has not stopped")
    names = set()
    if namespace.exists():
        if state["namespace_requested"] and "namespace_inode" not in state and not state["released"] and not state["move_requested"]:
            # Creation intent is durable but the add's checkpoint may be missing.
            # Only adopt the uniquely named, empty namespace before any NIC release.
            validate_namespace(namespace_links(state), command("ip", "netns", "pids", state["namespace"]).stdout.split())
            mark(directory, state, "namespace_inode", namespace_inode(namespace))
        if not state["namespace_requested"] or namespace_inode(namespace) != state.get("namespace_inode"):
            raise Refused("namespace ownership checkpoint is missing or changed; manual inspection required")
        holders = command("ip", "netns", "pids", state["namespace"]).stdout.split()
        if holders:
            raise Refused("processes still hold the receiver namespace")
        names = namespace_links(state)
        if names - {"lo", client}:
            raise Refused("unexpected interfaces in receiver namespace")
    if state["released"]:
        ensure_physical_instance(state)
        # NM can manage a returning interface before an explicit con-up. Validate
        # the persistent safety settings BEFORE crossing that boundary.
        original = state["snapshot"]["profile"]
        if profile(client, original["uuid"]) != original:
            raise Refused("original safe tether profile changed; leaving device isolated")
        if client in names:
            recorded = state["snapshot"]["device"]
            current = inspect_inside(directory, state)
            if "namespace_ifindex" not in recorded:
                # The move succeeded but the post-move checkpoint did not. The
                # owned namespace plus unchanged physical instance proves identity.
                if not state["move_requested"]:
                    raise Refused("interface move was not requested by this run")
                recorded["namespace_ifindex"] = current["ifindex"]
            verify_device(recorded, current, inside=True)
            command("ip", "-n", state["namespace"], "link", "set", "dev", client, "netns", "1")
        verify_device(state["snapshot"]["device"], device(client))
        wait_for_network_manager(state)
        command("nmcli", "--wait", "20", "connection", "up", "uuid", original["uuid"], "ifname", client, timeout=25)
        if command("nmcli", "-g", "GENERAL.CON-UUID", "device", "show", client).stdout.strip() != original["uuid"]:
            raise Refused("NetworkManager did not restore the original tether profile")
        # Re-verification is separate from restoration; a DNS blip must not cause
        # the next recovery attempt to unnecessarily reactivate the tether.
        mark(directory, state, "released", False)
    if namespace.exists():
        validate_namespace(namespace_links(state), command("ip", "netns", "pids", state["namespace"]).stdout.split())
        command("ip", "netns", "delete", state["namespace"])
    for name, inode in state["mountpoints"].items():
        path = Path(name)
        if not path.exists():
            continue
        metadata = path.lstat()
        if inode is None or not stat.S_ISDIR(metadata.st_mode) or metadata.st_ino != inode:
            state.setdefault("cleanup_notes", []).append("Unverified DHCP directory left untouched: " + name)
            continue
        # Only an empty directory created by this run; never recursive removal.
        try:
            path.rmdir()
        except OSError as error:
            state.setdefault("cleanup_notes", []).append(f"Could not remove empty DHCP directory {name}: {error}")


def require_root():
    if os.geteuid() != 0:
        raise Refused("network lifecycle operation requires explicit sudo")


def dispatch(operation, args):
    require_root()
    os.umask(0o077)
    if len(args) != 1:
        raise Refused("invalid internal invocation")
    value = args[0]
    if operation == "_prepare":
        return prepare(value)
    if operation == "_inspect":
        print(json.dumps(device(value)))
        return 0
    identifier = run_id(value)
    if operation == "_service":
        return service(identifier)
    if operation == "_receiver":
        return enter_receiver(identifier)
    if operation in ("_lease", "_lease-mounted"):
        return lease_process(identifier, operation == "_lease-mounted")
    if operation == "_cleanup":
        return cleanup(identifier)
    if operation == "_recover":
        directory, state = load(identifier)
        if int(os.environ.get("SUDO_UID", "0")) not in (0, state["config"]["uid"]):
            raise Refused("run belongs to another user")
        with record_interrupts(ignore=True):
            command("systemctl", "stop", unit_name(identifier), timeout=100, check=False)
            return cleanup(identifier)
    raise Refused("unknown internal operation")
