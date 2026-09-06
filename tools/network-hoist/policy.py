"""Validation and recovery decisions owned by the developer test launcher."""

import ipaddress
import re


class Refused(RuntimeError):
    """The launcher cannot safely establish or restore the requested topology."""


def interface_name(value):
    if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,14}", value):
        raise Refused("invalid network interface name")
    if value == "lo":
        raise Refused("loopback is not an uplink")
    return value


def run_id(value):
    if not re.fullmatch(r"[0-9a-f]{32}", value):
        raise Refused("invalid run ID")
    return value


def seconds(value):
    value = int(value)
    if not 1 <= value <= 3600:
        raise Refused("time budget must be between 1 and 3600 seconds")
    return value


def validate_profile(profile):
    """Only stable, safety-relevant profile fields participate in restoration."""
    if profile["unsaved"] or profile["flags"] != 0:
        raise Refused("tether profile must be saved, non-generated and non-volatile")
    if not profile["filename"].startswith("/etc/NetworkManager/system-connections/"):
        raise Refused("tether needs a persistent NetworkManager keyfile under /etc")
    if profile["bound_interface"] != profile["interface"]:
        raise Refused("tether profile must be bound to the selected interface")
    if any(value != "yes" for value in profile["routing_settings"].values()):
        raise Refused("tether profile must have IPv4/IPv6 never-default and ignore-auto-dns enabled")
    if profile["compatible_profiles"] != [profile["uuid"]]:
        raise Refused("another Ethernet profile could claim the tether during restoration")


def validate_routes(host, client, defaults, rules, resolver_routes):
    interface_name(host)
    interface_name(client)
    if host == client:
        raise Refused("--host and --client must be different interfaces")
    if not defaults["4"]:
        raise Refused("host requires an IPv4 default route")
    for family in ("4", "6"):
        for route in defaults[family]:
            if route.get("dev") != host or route.get("nexthops"):
                raise Refused("all host default routes must use --host, with no multipath")
        for rule in rules[family]:
            if rule.get("table") not in ("local", "main", "default", 255, 254, 253):
                raise Refused("custom policy routing is not supported by this test launcher")
            if rule.get("priority") not in (0, 32766, 32767):
                raise Refused("custom policy routing is not supported by this test launcher")
    if not resolver_routes:
        raise Refused("no host nameservers were found")
    for server, route in resolver_routes:
        address = ipaddress.ip_address(server)
        if address.is_loopback or route.get("dev") != host:
            raise Refused("host DNS must be directly reachable through --host, not the tether or a stub")


def dns_servers(value):
    result = []
    for word in value.split():
        address = ipaddress.IPv4Address(word)
        if address.is_unspecified or address.is_multicast or address.is_loopback:
            raise Refused("DHCP supplied an unusable nameserver")
        if str(address) not in result:
            result.append(str(address))
    if not result or len(result) > 8:
        raise Refused("DHCP must supply between one and eight usable nameservers")
    return result


def verify_device(recorded, current, *, inside=False):
    if current["device_path"] != recorded["device_path"] or current["device_event"] != recorded["device_event"]:
        raise Refused("selected physical device was replaced or rebound; refusing to move it")
    if inside:
        if current["ifindex"] != recorded["namespace_ifindex"]:
            raise Refused("namespace interface instance changed; refusing to move it")
    elif current["device_inode"] != recorded["device_inode"]:
        raise Refused("physical device was unplugged/replaced; manual recovery is required")


def validate_namespace(names, holders):
    if holders:
        raise Refused("processes still hold the namespace; refusing to restore or delete it")
    if set(names) - {"lo"}:
        raise Refused("unexpected interfaces remain in the namespace")
