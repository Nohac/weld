"""Shared run discovery and renderer-specific reports; no devices or builds."""
import json
import os
from pathlib import Path
import re
import runpy
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
PLOT = runpy.run_path(str(SCRIPTS / "plot-hoist"))
SOURCE = (
    '2026-09-26T12:00:00Z DEBUG encoded source observations interval_us=1000000 '
    'commits_received=60 commits_coalesced=2 batches_completed=58\n'
    '2026-09-26T12:00:00Z DEBUG Iroh selected path snapshot transport="ipv4" rtt_ms=4\n'
    '2026-09-26T12:00:01Z DEBUG encoded selected-path observations rtt_us=4000 '
    'path_epoch=1 lost_packets_total=0\n'
)
DESTINATION = (
    '2026-09-26T12:00:01Z DEBUG encoded destination observations interval_us=1000000 '
    'commits_received=58 media_received=58 decodes_completed=57 decodes_cancelled=1\n'
)


class SharedPlotTests(unittest.TestCase):
    def test_desktop_layouts_use_destination_and_do_not_invent_presentation(self):
        with tempfile.TemporaryDirectory() as temporary:
            for name in ("iroh-av1-direct.test", "headless-iroh-test", "network-hoist-test"):
                with self.subTest(name=name):
                    run = Path(temporary) / name
                    run.mkdir()
                    (run / "source.log").write_text(SOURCE)
                    (run / "destination.log").write_text(DESTINATION)
                    # Network launcher manifests contain more than diagnostic metadata.
                    (run / "run.json").write_text(json.dumps({"codec": "av1", "runtime": {
                        "environment": {"PRIVATE_MARKER": "must-not-be-embedded"}}}))
                    output = PLOT["report"](run)
                    self.assertIn("Codec: AV1 (requested; not confirmed in logs)", output)
                    self.assertIn("Direct IPv4 (observed)", output)
                    self.assertNotIn("must-not-be-embedded", output)
                    self.assertNotIn("Missing viewer.log", output)
                    self.assertNotIn("logcat", output)
                    self.assertNotIn("Legacy receiver outcomes", output)
                    self.assertNotIn("<h2>XR", output)
                    self.assertIn("Receiver presentation outcomes unavailable", output)
                    headings = re.findall(r"<h2>(.*?)</h2>", output)
                    self.assertEqual(headings[:2], ["Receiver decode throughput — not presentation", "Network RTT"])
                    chart = json.loads(re.search(r"class='chart-data'>(.*?)</script>", output)[1])
                    self.assertEqual(chart["labels"], ["Completed decodes"])
                    self.assertEqual(chart["measurements"][0][2], [57])

    def test_local_log_pair_and_both_cli_entrypoints_produce_same_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "weld-hoist-source-encoded-av1.log"
            receiver = source.with_name("weld-hoist-destination-encoded-av1.log")
            source.write_text(SOURCE)
            receiver.write_text(DESTINATION)
            self.assertEqual(PLOT["log_paths"](source), [source, receiver])
            for entry in ("plot-hoist", "plot-godot-hoist"):
                result = subprocess.run([sys.executable, str(SCRIPTS / entry), str(source), "--no-open"],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(source.with_suffix(".html").read_text(), PLOT["report"](source))

    def test_latest_run_uses_receiver_activity_across_all_supported_layouts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidates = []
            for index, name in enumerate(("godot-hoist-test", "iroh-h264-direct.test", "headless-iroh-test", "network-hoist-test")):
                run = root / name
                run.mkdir()
                (run / "source.log").write_text(SOURCE)
                os.utime(run / "source.log", (10, 10))
                receiver = run / ("viewer.log" if index == 0 else "destination.log")
                receiver.write_text(DESTINATION)
                os.utime(receiver, (100 + index, 100 + index))
                candidates.append((run, receiver))
            # A newer unrelated directory is not a hoist run.
            other = root / "unrelated"
            other.mkdir()
            (other / "source.log").write_text(SOURCE)
            self.assertEqual(PLOT["latest_run"](root), candidates[-1][0])
            os.utime(candidates[0][1], (200, 200))
            self.assertEqual(PLOT["latest_run"](root), candidates[0][0])
            source = root / "weld-hoist-source-native.log"
            source.write_text(SOURCE)
            self.assertEqual(PLOT["latest_run"](root), source)

    def test_restart_logs_remain_distinct_and_missing_metadata_is_not_logcat(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "source.log").write_text(SOURCE)
            (run / "destination.log").write_text(DESTINATION)
            restart = run / "restart1"
            restart.mkdir()
            (restart / "source.log").write_text(SOURCE.replace("12:00:", "12:01:"))
            records, notices, metadata = PLOT["read_records"](run)
            self.assertEqual(len([row for row in records if row["kind"] == "source"]), 2)
            self.assertFalse(notices)
            self.assertFalse(metadata)
            (run / "viewer.log").write_text(DESTINATION)
            with self.assertRaisesRegex(ValueError, "both viewer.log and destination.log"):
                PLOT["report"](run)

    def test_malformed_optional_metadata_and_missing_adb_route_do_not_break_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "source.log").write_text(SOURCE +
                '2026-09-26T12:00:01Z DEBUG Iroh ADB link observations adapter_dropped_total=0\n')
            (run / "destination.log").write_text(DESTINATION)
            for metadata in ("{", "[]", '{"started_at": null, "codec": []}'):
                (run / "run.json").write_text(metadata)
                output = PLOT["report"](run)
                self.assertIn("samples without a route ID omitted", output)
                self.assertIn("Receiver decode throughput", output)
                self.assertNotIn("<h2>ADB adapter queue drops", output)

    def test_xr_charts_require_actual_runtime_samples(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "source.log").write_text(SOURCE)
            viewer = run / "viewer.log"
            viewer.write_text(DESTINATION)
            self.assertNotIn("<h2>XR frame rate", PLOT["report"](run))
            with viewer.open("a") as log:
                log.write('1790424002.0 I PxrMetric: Pkg=com.example.weldvr,FPS=90,FrmCpu=2,FrmGpu=3,FrmLate=0\n')
            self.assertIn("<h2>XR frame rate", PLOT["report"](run))


if __name__ == "__main__":
    unittest.main()
