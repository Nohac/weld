"""The stress report must exclude warmup/tail and never call a partial run success."""
import json
from pathlib import Path
import runpy
import tempfile
import unittest

API = runpy.run_path(str(Path(__file__).with_name("run-godot-video-stress")))


def sample(elapsed, frames, panel=0):
    return "WELD_STRESS_SAMPLE " + json.dumps(dict(
        panel=panel, elapsed=elapsed, submitted=frames, decoded=frames, imported=frames,
        superseded=0, ticks=frames, queued_ticks=0, fence_ticks=0, empty_ticks=0,
        late_submissions=0, render_wait_us=frames * 20, import_us=frames * 200,
        selection_age_us=frames * 14000, selection_age_max_us=22000))


class ReportTests(unittest.TestCase):
    def report(self, lines, panels=1):
        with tempfile.TemporaryDirectory(prefix="weld-stress-report-") as directory:
            path = Path(directory) / "test.log"
            path.write_text("\n".join(lines))
            return API["summarize"](path, dict(seconds=10, panels=panels))

    def test_warmup_tail_and_other_app_metrics_do_not_skew_rates(self):
        result = self.report([
            "WELD_STRESS_START {}", sample(1, 0), sample(3, 270),
            "PxrMetric: FrmGpu=999ms,GPUTemp=999C,Pkg=com.other,",
            "PxrMetric: FrmGpu=8.2ms,GPUTemp=50C,Pkg=com.example.weldvr,",
            sample(8, 720), sample(13, 900), "WELD_STRESS_DONE success=true"])
        self.assertEqual(result["panels"][0]["imported_fps"], 90)
        self.assertEqual(result["panels"][0]["mean_selection_age_ms"], 14)
        self.assertEqual(result["pico_mean_gpu_ms"], 8.2)
        self.assertEqual(result["pico_max_gpu_temperature_c"], 50)

    def test_missing_or_failed_streams_cannot_pass(self):
        lines = ["WELD_STRESS_START {}", sample(3, 270), sample(8, 720)]
        for tail, panels in (([], 1), (["WELD_STRESS_DONE success=false"], 1),
                             (["WELD_STRESS_DONE success=true"], 2)):
            with self.subTest(tail=tail, panels=panels), self.assertRaises(RuntimeError):
                self.report(lines + tail, panels)


if __name__ == "__main__":
    unittest.main()
