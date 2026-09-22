"""Counter semantics, clock parsing, honest missing data, and bounded capture."""
import io
import json
import math
from datetime import datetime, timezone
from pathlib import Path
import runpy
import re
import tempfile
import unittest
from unittest.mock import patch

PLOT = runpy.run_path(str(Path(__file__).with_name("plot-godot-hoist")))
CAPTURE = runpy.run_path(str(Path(__file__).with_name("godot-log-capture.py")))


def row(time, count, epoch=1, file="source.log"):
    return dict(time=time, file=file, fields=dict(count=count, epoch=epoch))


class DiagnosticsTests(unittest.TestCase):
    def test_adb_paths_and_queue_drops_are_distinct_from_quic_loss(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "run.json").write_text(json.dumps({"transport": "adb"}))
            (run / "source.log").write_text(
                '2026-09-21T12:00:01Z DEBUG Iroh selected path snapshot transport="adb" rtt_ms=2\n'
                '2026-09-21T12:00:01Z DEBUG Iroh ADB link observations route=1 adapter_dropped_total=10\n'
                '2026-09-21T12:00:06Z DEBUG Iroh ADB link observations route=1 adapter_dropped_total=15\n')
            output = PLOT["report"](run)
            self.assertIn("ADB byte stream (observed)", output)
            self.assertIn("not USB loss", output)
            self.assertIn("ADB adapter queue drops", output)
            (run / "run.json").write_text(json.dumps({"transport": "adb-loopback"}))
            self.assertIn("Custom TCP loopback (no USB)", PLOT["report"](run))

    def test_backend_and_receiver_logs_supply_distinct_pipeline_charts(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "source.log").write_text(
                "2026-09-21T12:00:01Z DEBUG encoded source observations interval_us=2000000 "
                "commits_received=240 commits_coalesced=40 batches_completed=200 "
                "layer_frames_completed=200 encoded_payload_bytes=2000000 "
                "batch_wall_max_us=9000 active_batch_age_us=2000 pending_events=3 retained_output_records=2\n"
                "2026-09-21T12:00:01Z DEBUG encoded selected-path observations rtt_us=7000 "
                "path_epoch=1 lost_packets_total=0\n"
                "2026-09-21T12:00:02Z DEBUG encoded selected-path observations rtt_us=8000 "
                "path_epoch=1 lost_packets_total=0\n")
            (run / "viewer.log").write_text(
                "2026-09-21T12:00:01Z DEBUG encoded destination observations interval_us=1000000 "
                "commits_received=100 media_received=100 decodes_completed=99 "
                "pending_events=1 pending_media_frames=0 decode_jobs_in_flight=1\n")
            output = PLOT["report"](run)
            charts = [json.loads(payload) for payload in re.findall(r"class='chart-data'>(.*?)</script>", output)]
            by_label = {item["labels"][0]: item for item in charts}
            self.assertEqual(by_label["Application commits received"]["measurements"][0][2], [120, 20, 100])
            self.assertEqual(by_label["Encoded payload"]["measurements"][0][2], [8])
            self.assertEqual(by_label["Received commits"]["measurements"][0][2], [100, 100, 99])
            self.assertEqual(by_label["Round-trip time"]["measurements"][0][2], [7])
            self.assertEqual(by_label["Packets declared lost"]["measurements"][0][2], [0])
            self.assertNotIn("Network RTT and packet-loss measurements unavailable", output)
            self.assertIn("Weld backend media-send queues unavailable", output)

    def test_report_codec_comes_from_actual_encoders_before_requested_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "run.json").write_text(json.dumps({"codec": "av1", "started_at": 1789815600}))
            (run / "viewer.log").write_text("")
            (run / "source.log").write_text(
                "2026-09-19T12:00:00Z INFO opened VA-API encoder generation codec=H264 stream=1\n")
            self.assertIn("Codec: H.264 (encoder logs)", PLOT["report"](run))
            with (run / "source.log").open("a") as log:
                log.write("2026-09-19T12:00:01Z INFO opened VA-API encoder generation codec=Av1 stream=2\n")
            self.assertIn("Codec: AV1, H.264 (encoder logs)", PLOT["report"](run))
            (run / "source.log").write_text(
                "2026-09-19T12:00:00Z DEBUG presentation observations decoded_total=1\n")
            self.assertIn("Codec: AV1 (requested; not confirmed in logs)", PLOT["report"](run))
            (run / "run.json").write_text("{}")
            self.assertIn("Codec: Unknown — not recorded", PLOT["report"](run))

    def test_report_opens_by_default_and_no_open_skips_launch(self):
        with tempfile.TemporaryDirectory(prefix="weld report ") as temporary:
            run = Path(temporary)
            with patch.dict(PLOT["main"].__globals__, report=lambda _: "<html>report</html>"):
                for flags in ([], ["--no-open"]):
                    with patch.object(PLOT["subprocess"], "run") as launch:
                        PLOT["main"]([str(run), *flags])
                        self.assertEqual((run / "diagnostics.html").read_text(), "<html>report</html>")
                        if flags:
                            launch.assert_not_called()
                        else:
                            launch.assert_called_once_with(
                                ["xdg-open", str((run / "diagnostics.html").resolve())], check=True, timeout=10)

    def test_missing_opener_preserves_report_and_prints_warning(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            with patch.dict(PLOT["main"].__globals__, report=lambda _: "report"), \
                    patch.object(PLOT["subprocess"], "run", side_effect=FileNotFoundError("xdg-open")), \
                    patch("sys.stderr", new_callable=io.StringIO) as errors:
                PLOT["main"]([str(run)])
                self.assertEqual((run / "diagnostics.html").read_text(), "report")
                self.assertIn("Report saved, but could not open it", errors.getvalue())

    def test_cumulative_deltas_rebaseline_resets_epochs_and_process_files(self):
        rows = [row(0, 10), row(2, 18), row(3, 100, 2), row(4, 103, 2),
                row(5, 1, 2), row(6, 3, 2), row(7, 99, 2, "restart.log")]
        self.assertEqual(PLOT["cumulative"](rows, ["count"], "epoch"),
                         [(0, 2, [4]), (3, 4, [3]), (5, 6, [2])])

    def test_missing_field_is_not_a_zero_loss_sample(self):
        rows = [row(0, 10), dict(time=1, file="source.log", fields={}), row(2, 20)]
        self.assertEqual(PLOT["cumulative"](rows, ["count"]), [])
        self.assertEqual(PLOT["sampled"](rows[1:2], ["count"]), [])
        self.assertIn("Unavailable", PLOT["chart"]("loss", ["packets"], [], 0, 10, "packets/s"))

    def test_chart_intervals_keep_shared_origin_gaps_and_adjacent_rates(self):
        data = PLOT["chart_data"]([(101, 102, [4]), (102, 103, [5]), (105, 106, [0])], 100, 1)
        samples = dict(zip(*data))
        self.assertEqual(samples[1], 4)
        self.assertEqual(samples[math.nextafter(2, -math.inf)], 4)
        self.assertEqual(samples[2], 5)
        self.assertIsNone(samples[3])
        self.assertEqual(samples[5], 0)
        self.assertIsNone(samples[6])
        self.assertEqual(data[0], sorted(set(data[0])))

    def test_overlapping_display_spans_do_not_insert_false_gaps(self):
        data = PLOT["chart_data"]([(0, 2, [1]), (1, 3, [2])], 0, 1)
        self.assertEqual(data[1], [1, 1, 2, 2, None])

    def test_embedded_chart_json_cannot_close_script(self):
        output = PLOT["chart"]("<unsafe>", ["</script><script>alert(1)</script>"],
                               [(0, 1, [3])], 0, 2, "ms")
        self.assertNotIn("<unsafe>", output)
        self.assertEqual(output.count("</script>"), 1)
        payload = re.search(r"class='chart-data'>(.*?)</script>", output)[1]
        self.assertEqual(json.loads(payload)["labels"], ["</script><script>alert(1)</script>"])

    def test_rtt_immediately_follows_frame_outcomes_in_modern_report(self):
        records = [dict(time=100, kind="presentation", fields={}, file="viewer.log"),
                   dict(time=101, kind="network", fields={"rtt_us": 8000}, file="source.log")]
        with patch.dict(PLOT["report"].__globals__, read_records=lambda _: (records, [])):
            output = PLOT["report"](Path("test-run"))
        self.assertEqual(re.findall(r"<h2>(.*?)</h2>", output)[:2],
                         ["Receiver frame outcomes", "Network RTT"])
        self.assertEqual(output.count("<h2>Network RTT</h2>"), 1)
        self.assertIn("uplot@1.6.32/dist/uPlot.iife.min.js", output)
        self.assertIn("could not load uPlot from the CDN", output)
        self.assertIn('sync: {key: "weld-run", setSeries: false}', output)

    def test_decoded_rate_is_separate_from_outcomes_when_recorded(self):
        fields = dict.fromkeys(["imported_total", "superseded_total", "stale_total",
                               "layout_discard_total", "lifecycle_discard_total", "handoff_discard_total"], 0)
        records = [dict(time=100, kind="presentation", fields=dict(fields, decoded_total=10), file="viewer.log"),
                   dict(time=102, kind="presentation", fields=dict(fields, decoded_total=30), file="viewer.log")]
        with patch.dict(PLOT["report"].__globals__, read_records=lambda _: (records, [])):
            output = PLOT["report"](Path("test-run"))
        payload = json.loads(re.search(r"class='chart-data'>(.*?)</script>", output)[1])
        self.assertEqual(payload["labels"][-1], "Decoded (not an additional outcome)")
        self.assertEqual(payload["data"][-1], [10, 10, None])

    def test_inner_utc_wins_over_delayed_logcat_delivery(self):
        instant = datetime(2026, 9, 16, 20, 0, tzinfo=timezone.utc).timestamp()
        self.assertEqual(PLOT["timestamp"]("1790000000.123 I godot: 2026-09-16T20:00:00Z DEBUG", 2026), instant)
        self.assertEqual(PLOT["timestamp"]("1790000000.123 I godot: message", 2026), 1790000000.123)
        self.assertIsNone(PLOT["timestamp"]("untimestamped", 2026))
        self.assertEqual(PLOT["timestamp"]("   1790000000.123 I godot: message", 2026), 1790000000.123)

    def test_android_fragments_reassemble_and_missing_pieces_are_explicit(self):
        lines = ["prefix WELD_TRACE id=1 part=1 total=2 hello ",
                 "prefix WELD_TRACE id=2 part=1 total=1 other", "prefix WELD_TRACE id=1 part=2 total=2 world"]
        self.assertEqual(list(PLOT["joined_lines"](lines)), ["other\n", "hello world\n"])
        self.assertIn("incomplete_records=1", ''.join(PLOT["joined_lines"](lines[:1])))

    def test_bounded_capture_drains_input_and_marks_loss(self):
        source = io.BytesIO(b"abcdefgh\n" * 100)
        destination = io.BytesIO()
        self.assertTrue(CAPTURE["copy_bounded"](source, destination, maximum=128))
        self.assertLessEqual(len(destination.getvalue()), 128)
        self.assertIn(b"WELD_CAPTURE_GAP", destination.getvalue())
        self.assertEqual(source.read(), b"")

    def test_legacy_report_does_not_invent_stage_or_network_measurements(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            (run / "source.log").write_text("")
            (run / "viewer.log").write_text(
                "09-16 22:00:00.000 I godot: WELD_HOIST_STATUS 1 layers, decoded 100, presented 90, superseded 10\n"
                "09-16 22:00:01.000 I godot: WELD_HOIST_STATUS 1 layers, decoded 190, presented 175, superseded 15\n")
            report = PLOT["report"](run)
            self.assertIn("unknown reason", report)
            self.assertIn("Unavailable", report)
            self.assertIn("Network RTT and packet-loss measurements unavailable", report)
            self.assertIn("Weld backend/source timing unavailable", report)
            payload = re.search(r"class='chart-data'>(.*?)</script>", report)[1]
            self.assertIn(85, json.loads(payload)["data"][1])
            self.assertEqual(re.findall(r"<h2>(.*?)</h2>", report)[:2],
                             ["Legacy receiver outcomes — discard reasons unavailable", "Network RTT"])


if __name__ == "__main__":
    unittest.main()
