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

The final traffic summary shows RX, TX, combined bytes (decimal MB), and average
Mbps separately for the receiver interface and the host uplink. Samples start
after initial DHCP acquisition, before Weld startup, and end before teardown.
Missing/reset counters are reported as unavailable, not zero. These read-only
diagnostics do not affect routing or recovery.

The isolated receiver's totals include transport overhead and discovery traffic.
The host uplink also includes application downloads and unrelated host traffic;
neither row is exact per-Weld-process accounting. Do not sum the two rows as
unique data, since the same stream crosses both interfaces. systemd's separate
IP Traffic summary counts both Weld processes and their child applications over
the service lifetime; a browser downloading video can make received bytes much
higher than sent bytes. Its IO Bytes field measures storage I/O, not networking.
