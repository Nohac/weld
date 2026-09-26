# Shared hoist diagnostics

`scripts/plot-hoist` generates an HTML/uPlot report for encoded Weld-to-Weld,
headless-to-Weld and Godot (desktop or XR) runs. It uses the same existing
source, transport and decoder records; it does not add a second telemetry path.

```sh
# Latest recognized run; opens the report with xdg-open.
scripts/plot-hoist
# A particular run; write HTML without opening a browser.
scripts/plot-hoist target/validation/iroh-av1-direct.RUN --no-open
scripts/plot-hoist target/validation/headless-iroh-RUN --no-open
scripts/plot-hoist target/validation/network-hoist-RUN --no-open
scripts/plot-hoist target/validation/godot-hoist-RUN --no-open
# The local Unix launcher retains flat source/destination log pairs.
scripts/plot-hoist target/validation/weld-hoist-source-encoded-av1.log --no-open
```

Replace `RUN` with the actual directory name printed by the launcher. Explicit
directories need not use those names. Automatic discovery searches those run
families and local source-log pairs under `target/validation`, choosing the
newest source/receiver log modification time, not the newest generated report.
The old `scripts/plot-godot-hoist` command is an alias with the same arguments
and discovery behavior, including Weld-to-Weld runs.

Directory runs use `source.log` and either `destination.log` or `viewer.log`.
If both receiver names exist, plotting refuses rather than blending two
receivers. Existing `restart*/source.log` and receiver logs are included with
separate file identities for counter resets. Local log pairs write the report
next to the selected source log with an `.html` suffix; directory runs write
`diagnostics.html`. `--output` overrides either location.

## Recording useful data

The normal Iroh, headless Iroh and local-hoist launchers now enable
`weld_media_diag=debug` and `weld_network_diag=debug` summaries by default. The
isolated-network launcher already does so. Explicit `RUST_LOG` values remain
authoritative; include these targets if overriding the defaults:

```sh
RUST_LOG=warn,weld_media_diag=debug,weld_network_diag=debug scripts/run-iroh-hoist --codec av1
```

The Godot launcher also enables its presentation summaries. These are summary
records, not video dumps or per-frame trace logging. Older runs without the
targets enabled may have partial data or no plottable samples. Native-buffer
local hoist does not generate encoded-media measurements just because its
filename is recognized. Missing data is not synthesized.

New ordinary/headless Iroh runs record requested codec, transport, receiver and
start time in `run.json`. Older runs work without it. Actual encoder/path logs
take precedence over requested metadata. The plotter reads only `run.json` for
optional metadata, never private pairing files or `runtime.json`, and does not
embed the network launcher's environment snapshot in the HTML. Malformed
optional metadata produces a warning without discarding usable log records.

## Measurement boundaries

- Common charts cover source commits/coalescing, encode work, encoded payload,
  receiver ingress/decode, queue ages, media-write backlog/cancellation, RTT and
  QUIC loss when those measurements exist.
- Godot presentation records add imported, superseded, stale, layout/lifecycle
  and handoff outcomes. They are not hardware scanout counts. Desktop Weld
  decode completion is likewise **not** proof of display presentation; absent
  presentation/superseded counters are explicitly unavailable.
- The first chart is receiver frame outcomes when available, otherwise decode
  throughput. Network RTT is immediately below it. XR runtime charts appear
  only when actual Pico runtime samples are present.
- Shared time cursor/zoom, whole-run and visible-range averages, measured totals
  and Y-axis locking retain the existing chart behavior. Counter resets and
  missing coverage must not turn into invented zero-loss intervals.
- Cross-device wall clocks are approximate; stage timings overlap. The report
  does not derive one-way network latency or add unlike discard counts into a
  single loss total.

Reports keep measurements embedded locally and load pinned uPlot assets from a
CDN; viewing needs Internet access or cached assets, not a log-upload service.
Each log is limited to 64 MiB, run metadata to 1 MiB and parsed diagnostic
records to 20,000. Select shorter runs when those bounds are exceeded.

ADB remains a development carrier. Its current listener retires on any accept
error, including transient errors, until the source host restarts. Retry/backoff,
broader recovery and transport-specific allocation tuning are deferred. This
limitation is not a diagnosis of periodic Wi-Fi stalls.
