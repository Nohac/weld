# Iroh over ADB experiment

Standalone synthetic transport probe. It does not modify Weld XR, carry application
data, or exercise codecs, rendering, or presentation timing.

From the repository root:

```sh
scripts/run-iroh-adb-probe --desktop --seconds 5
scripts/run-iroh-adb-probe --serial PA921CMGK7210024G --seconds 5
```

Requires the existing Rust/Android Nix environment and an authorized ADB device.
The default ADB path deliberately matches the development environment's shared
server; override with `--adb` if needed. The launcher builds only this standalone
workspace, using the dev profile, two jobs, and `target/iroh-adb-probe/cargo`.
It does not run tests or restart the ADB server.

Each mode runs idle, 24 Mbps and 96 Mbps phases, emitting synthetic frames at
90 Hz while a separate logical flow measures sequence-checked echo round trips.
The raw baseline is **unauthenticated synthetic data only**. Iroh uses disposable
authenticated identities, separate QUIC streams, and only the custom transport:
IP transports, relays, discovery, and port mapping are disabled. The selected
path and peer identity are checked before the workload.

The Android binary runs as the **ADB shell UID**, not the Godot app UID. This is
transport feasibility evidence, not an end-to-end Android receiver validation.
ADB transports an ordered byte stream; retaining Iroh does not remove underlying
head-of-line blocking or make it behave like native UDP.

## Bounds and observations

- Phases are limited to 1–10 seconds each; device process timeout is 50 seconds.
- Core dumps are disabled. Per-process output files are limited to 1 MiB.
- Packet records are limited to 65535 bytes, synthetic frames to 256 KiB.
- Iroh send queue: 256 packets; receive queue: 32; raw queues: 32 records each.
- Private run directories and identity files; keys removed on completion.
- Only this run's processes, device files and confirmed reverse mapping are cleaned up.
- JSONL logs remain in `target/validation/iroh-adb-probe-*`.
- Sender enqueue counts are not socket-completion counts. Packets can cross phase
  boundaries. Adapter queue drops and QUIC loss detection are separate metrics;
  neither should automatically be described as USB packet loss.
- CPU measurements are process clock ticks; the launcher prints ticks/second.

Iroh 1.1.0's custom send dispatch does not propagate `Poll::Pending` correctly.
This experiment uses counted drop-on-full, as its upstream test transport does,
rather than claiming to apply backpressure. A 32-slot send queue itself dropped
packets at 96 Mbps even on loopback: a 90 Hz frame produces roughly 112 initial-MTU
datagrams. Increasing only the send queue to 256 removed those artificial drops.
See pinned Iroh `src/socket/transports.rs`, custom send dispatch, and
`src/test_utils/test_transport.rs`.

Use explicit device ports with `--no-rebind`, **not `tcp:0`**. ADB 37.0.1's
host-side reverse authorization records the literal `tcp:0` request, whereas
removal names the allocated port. A subsequent no-rebind request can leave the
new destination unregistered and abort the shared server. The launcher chooses
a high explicit port, verifies the mapping, and never clears unrelated entries.
Relevant upstream code: [reverse configuration tracking](https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/transport.cpp)
and [incoming connection authorization](https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/adb.cpp).
These moving references were inspected on 2026-09-21; the fatal message also
appeared in the local 37.0.1 server log during reproduction.

## Initial Pico result, 2026-09-21

Run `iroh-adb-probe-z62cw8e0`, five seconds per phase, debug opt-level 1:

| Target | Iroh received | Echo median | Echo p99 |
| --- | --- | --- | --- |
| Idle | — | 2.18 ms | 42.30 ms |
| 24 Mbps | 24.07 Mbps | 2.74 ms | 23.87 ms |
| 96 Mbps | 95.89 Mbps | 5.00 ms | 9.91 ms |

All payloads arrived intact, zero adapter drops, zero reported QUIC loss, clean
exit. The raw baseline also sustained both rates but had 16–40 ms p99 tails.
This establishes throughput feasibility, **not consistently low latency**.
The short test does not cover the roughly five-minute Wi-Fi stall interval.
Per-record framing currently uses separate header/payload writes, and ADB has
its own buffering; these are candidates for controlled latency experiments,
not established explanations for the tails. No production integration yet.

Checks:

```sh
CARGO_TARGET_DIR=target/iroh-adb-probe/cargo cargo test --manifest-path tools/iroh-adb-probe/Cargo.toml --locked -j2
CARGO_TARGET_DIR=target/iroh-adb-probe/cargo cargo clippy --manifest-path tools/iroh-adb-probe/Cargo.toml --locked --all-targets -j2 -- -D warnings
python3 -m unittest scripts/test_iroh_adb_probe.py
```

Rust tests require loopback socket access. The Python tests do not access ADB.
