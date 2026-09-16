"""Counter semantics, clock parsing, honest missing data, and bounded capture."""
import io
from datetime import datetime, timezone
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

PLOT = runpy.run_path(str(Path(__file__).with_name("plot-godot-hoist")))
CAPTURE = runpy.run_path(str(Path(__file__).with_name("godot-log-capture.py")))


def row(time, count, epoch=1, file="source.log"):
    return dict(time=time, file=file, fields=dict(count=count, epoch=epoch))


class DiagnosticsTests(unittest.TestCase):
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
            self.assertIn("85.000 frames/s", report)
            self.assertNotIn("<script", report)


if __name__ == "__main__":
    unittest.main()
