import json
import tempfile
import unittest
from pathlib import Path

import report


def phase(name, ops_per_second, p99, wa, daemon_cpu=1.0):
    return {"name": name, "operations": 1000, "operations_per_second": ops_per_second,
            "write_amplification": wa, "daemon_cpu_seconds": daemon_cpu, "client_cpu_seconds": 0.5,
            "latency": {"UPDATE": {"p50": 10, "p99": p99, "p999": p99 * 2}, "READ": {"p99": 5}}}


def summary(result, *phases):
    return {"result": result, "config": {"workload": "a"}, "facts": {"stop_reason": "steady"},
            "phases": list(phases)}


class ReportTest(unittest.TestCase):
    def test_measures_come_from_the_last_two_windows(self):
        run = summary("PASS", phase("calibration", 1, 1, 9), phase("window-01", 100, 50, 3.0),
                      phase("window-02", 200, 100, 2.0), phase("window-03", 400, 300, 1.0))
        measures = report.run_measures(run)
        self.assertEqual(measures["tps"], 300)
        self.assertEqual(measures["write_p99"], 200)
        self.assertEqual(measures["write_amplification"], 1.5)
        self.assertEqual(measures["daemon_cpu_us"], 1000)
        self.assertEqual(measures["client_cpu_us"], 500)

    def test_baseline_runs_have_no_daemon_cpu(self):
        run = summary("PASS", phase("window-01", 100, 50, 1.0, daemon_cpu=None))
        self.assertIsNone(report.run_measures(run)["daemon_cpu_us"])

    def test_aggregate_is_median_and_range_of_present_values(self):
        cell = report.aggregate([3.0, None, 1.0, 2.0])
        self.assertEqual((cell.median, cell.low, cell.high, cell.count), (2.0, 1.0, 3.0, 3))
        self.assertIsNone(report.aggregate([None]))

    def test_page_has_a_row_per_backing_and_a_chart_per_measure(self):
        with tempfile.TemporaryDirectory() as directory:
            matrix = Path(directory)
            order = ["raw-lv-a-r1", "thin-xfs-a-r1", "raw-lv-a-r2"]
            (matrix / "matrix.json").write_text(json.dumps({
                "variants": {"raw-lv": "ours: raw LV", "thin-xfs": "baseline: XFS on LVM thin"},
                "workloads": ["a"], "repeats": 2, "order": order}))
            for name, result in zip(order, ["PASS", "PASS", "FAIL"], strict=True):
                (matrix / "runs" / name).mkdir(parents=True)
                (matrix / "runs" / name / "summary.json").write_text(json.dumps(
                    summary(result, phase("window-01", 100, 50, 2.0), phase("window-02", 120, 60, 2.0))))
            page = report.write(matrix).read_text()
            self.assertIn("<th>ours: raw LV</th><td>1/2</td>", page)
            self.assertIn("<th>baseline: XFS on LVM thin</th><td>1/1</td>", page)
            self.assertEqual(page.count("<svg"), len(report.MEASURES))


if __name__ == "__main__":
    unittest.main()
