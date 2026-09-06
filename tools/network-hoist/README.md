# Isolated network-test launcher

Entry point: `scripts/run-network-hoist`. Read
[the validation guide](../../docs/network-validation.md#batch-2-isolated-launcher-with-bounded-recovery)
before a privileged run. The first target is NetworkManager-managed Wi-Fi on
the host and a separate physical USB/Ethernet receiver uplink.

- `main.py`: unprivileged CLI/build/runtime capture and privilege-drop entry.
- `lifecycle.py`: root-owned snapshot, bounded systemd lifecycle, private DHCP
  and resolver, conservative recovery.
- `policy.py`: supported-topology and device/profile identity rules.

The unit tests exercise our policy and recovery sequencing with fake commands;
they do not test Linux, DHCP, NetworkManager, or systemd implementations:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/network-hoist/tests -v
bash -n scripts/run-network-hoist
scripts/run-network-hoist --help
```

No namespace creation or interface mutation is part of those checks. Read-only
preflight additionally requires a real host terminal, not a container or the
agent's process-isolated sandbox. The privileged path still requires manual
validation and explicit approval of the exact interface move.
