import importlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import evidence

test_baseline = importlib.import_module("test_baseline")

SCRIPT = Path(__file__).with_name("workbench.py")


class ReportTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def cli(self, *args, status=0):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), *map(str, args)],
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )
        self.assertEqual(result.returncode, status, result.stdout + result.stderr)
        return result

    def sqlite_fixture(self):
        root = test_baseline.make_run(self.root / "sqlite")
        path = root / "manifest.json"
        value = json.loads(path.read_text())
        value["status"] = "pass"
        path.write_text(json.dumps(value))
        return root

    def timing_fixture(self):
        root = self.root / "timing"
        root.mkdir()
        case = {
            "driver": "rocksdb",
            "workload": "f",
            "load": {"process_elapsed_seconds": 1, "reported_counts": {"INSERT": 8}},
            "run": {
                "process_elapsed_seconds": 2,
                "usage": {"max_rss_kib": 1024, "fs_inputs": 99},
                "reported_counts": {"READ": 100, "UPDATE": 50, "TOTAL": 150},
            },
            "verification": {"row_count_output": "Keys in range: 8"},
        }
        report = {
            "scope": "duration calibration",
            "recordcount": 8,
            "operationcount": 100,
            "field_bytes": 4096,
            "threads": 1,
            "seed": 123,
            "status": "pass",
            "fixture_removed": True,
            "cases": [case],
        }
        (root / "report.json").write_text(json.dumps(report))
        (root / "rocksdb-f-run.log").write_text(
            "TOTAL - Takes(s): 1, Count: 50, 99th(us): 1, 99.99th(us): 2\n"
            "Run finished, takes 2s\n"
            "TOTAL - Takes(s): 2, Count: 150, 99th(us): 10, 99.99th(us): 20\n"
        )
        return root

    def test_full_matrix_preserves_samples_and_unavailable_metrics(self):
        run = self.sqlite_fixture()
        output = self.root / "out"
        self.cli("report", run, "--output", output)
        report = json.loads((output / "report.json").read_text())
        self.assertEqual(report["runs"][0]["status"], "validated-full-matrix")
        self.assertEqual(len(report["runs"][0]["rows"]), 87)
        speedtest = report["runs"][0]["rows"][0]
        self.assertIsNone(speedtest["ops_s"])
        self.assertIsNone(speedtest["physical_used_mib"])
        self.assertEqual(len((output / "cases.csv").read_text().splitlines()), 88)
        self.cli("report", run, "--output", output, status=2)

    def test_backing_io_distinguishes_zero_missing_and_flushes(self):
        root = self.sqlite_fixture()
        path = next(root.glob("*/result.jsonl"))
        value = json.loads(path.read_text())
        counters = {
            "backing_read_completed_bytes": 0,
            "backing_write_completed_bytes": 1048576,
            "backing_completed_reads": 0,
            "backing_completed_writes": 2,
            "backing_completed_flushes": 1,
        }
        segments = {
            name: dict(counters)
            for name in (
                "before_tool_to_drain_start_workload",
                "drain_start_to_drain_end",
                "drain_end_to_parent_after_residual",
            )
        }
        segments["segments_are_non_overlapping"] = True
        value["phases"]["workload_plus_drain"]["io_segments"] = segments
        path.write_text(json.dumps(value) + "\n")
        row = evidence.sqlite_row(root, path.parent.name, "ext4_matrix")
        self.assertEqual((row["backing_read_mib"], row["backing_write_mib"]), (0, 3))
        self.assertEqual((row["backing_requests"], row["backing_flushes"]), (6, 3))
        self.assertIsNone(row["physical_used_mib"])
        segments["drain_start_to_drain_end"]["backing_completed_reads"] = -1
        path.write_text(json.dumps(value) + "\n")
        with self.assertRaises(ValueError):
            evidence.sqlite_row(root, path.parent.name, "ext4_matrix")

    def test_missing_result_cannot_pass_and_is_visible(self):
        run = self.sqlite_fixture()
        next(run.glob("*/result.jsonl")).unlink()
        output = self.root / "out"
        self.cli("report", run, "--output", output, status=1)
        report = json.loads((output / "report.json").read_text())
        self.assertEqual(report["runs"][0]["status"], "invalid")
        self.assertEqual(
            sum(row["status"] == "missing" for row in report["runs"][0]["rows"]), 1
        )

    def test_timing_uses_logical_f_requests_and_final_latency_summary(self):
        run = self.timing_fixture()
        row = evidence.read_run(run)["rows"][0]
        self.assertEqual((row["operations"], row["ops_s"]), (100, 50))
        self.assertEqual((row["p99_api_us"], row["p9999_api_us"]), (10, 20))
        self.assertEqual(row["api_latencies"]["TOTAL"]["count"], 150)
        self.assertIsNone(row["backing_read_mib"])
        self.assertEqual(row["peak_rss_mib"], 1)

    def test_bad_latency_counts_and_partial_timing_remain_invalid(self):
        run = self.timing_fixture()
        log = run / "rocksdb-f-run.log"
        log.write_text(log.read_text().replace("Count: 150", "Count: 149"))
        output = self.root / "out"
        self.cli("report", run, "--output", output, status=1)
        result = json.loads((output / "report.json").read_text())
        self.assertIn("count mismatch", result["runs"][0]["rows"][0]["issues"][0])
        value = json.loads((run / "report.json").read_text())
        del value["cases"][0]["run"]
        (run / "report.json").write_text(json.dumps(value))
        self.assertEqual(evidence.read_run(run)["status"], "invalid")

    def test_filter_duplicate_input_and_matrix_readiness(self):
        run = self.timing_fixture()
        self.cli("report", run, run, "--output", self.root / "duplicate", status=2)
        self.cli(
            "report", run, "--case", "a", "--output", self.root / "empty", status=2
        )
        self.assertFalse((self.root / "empty").exists())
        result = self.cli("matrix")
        self.assertIn("48 runs", result.stdout)
        self.assertIn("not_ready", result.stdout)
        self.assertIn("ours/v1.0/xfs", result.stdout)

    def test_completed_cases_do_not_upgrade_an_incomplete_batch(self):
        run = self.timing_fixture()
        path = run / "report.json"
        value = json.loads(path.read_text())
        del value["status"]
        path.write_text(json.dumps(value))
        output = self.root / "out"
        self.cli("report", run, "--case", "f", "--output", output, status=1)
        result = json.loads((output / "report.json").read_text())["runs"][0]
        self.assertEqual(result["status"], "incomplete")
        self.assertEqual(result["rows"][0]["operations"], 100)

    def test_nonfinite_and_negative_metrics_are_rejected(self):
        for value in (float("nan"), float("inf"), -1, True):
            with self.assertRaises(ValueError):
                evidence.number(value)
        self.assertEqual(evidence.scaled(0, 1024), 0)
        self.assertIsNone(evidence.scaled(None, 1024))


if __name__ == "__main__":
    unittest.main()
