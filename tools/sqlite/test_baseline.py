#!/usr/bin/env python3
import copy
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("baseline.py")
PROFILES = ("delete-extra", "wal-full", "wal-normal")
CASES = ("speedtest-main", "kv-sequential-read-small", "kv-random-read-small",
         "kv-sequential-read-large", "kv-random-read-large",
         "kv-sequential-update-small-single-transaction",
         "kv-sequential-update-small-per-op-commit",
         "kv-random-update-large-single-transaction",
         "kv-random-update-large-per-op-commit")
GROUPS = [(case, profile) for profile in PROFILES for case in CASES]
GROUPS += [("kv-long-wal-checkpoint-stress", profile) for profile in PROFILES[1:]]


def result(case, profile, repetition, multiplier=1):
    unavailable = {"available": False, "reason": "upstream tool does not report it"}
    operation_rate = unavailable if case == "speedtest-main" else 10.0 * repetition * multiplier
    return {
        "schema": "block-storage-sqlite", "schema_version": 2,
        "scope": "ext4_matrix", "status": "pass", "errors": [],
        "labels": {"fs": "Ext4", "case": case, "profile": profile,
                   "repetition": repetition, "expected": 3},
        "parameters": {"tool": "speedtest1" if case == "speedtest-main" else "kvtest",
                       "operations": 30},
        "cache_condition": "warm-from-initialization; no global cache changes",
        "mount_options": ["filesystem=ext4 mount_options=rw,nosuid,nodev,relatime"],
        "phases": {"workload_plus_drain": {
            "timings": {"workload_ns": 100 * repetition * multiplier,
                        "workload_plus_drain_ns": 120 * repetition * multiplier},
            "operations_per_second": operation_rate}},
        "measurement_gaps": {"latency_tail_percentiles": unavailable},
        "validations": [
            {"phase": phase, "integrity_check": "ok", "foreign_key_rows": 0,
             "reference_match": True, "sha3sum_schema": "abc"}
            for phase in ("validation", "reopen_revalidation")],
        "metadata": {
            "final_ledger": {"valid": True, "pending_operations": 0, "errors": 0,
                             "write_limit_bytes": 512 * 1024 * 1024,
                             "write_reserved_bytes": 8192, "write_completed_bytes": 8192,
                             "completed_totals_incomplete": False,
                             "budget_exhausted": False},
            "tested_source": {"commit": "a" * 40, "dirty": False,
                              "kernel": "Linux test", "backing_medium": "tmpfs",
                              "geometry": {"logical_block_bytes": 4096},
                              "hardware": {"model": "test", "online_cpus": 8,
                                           "mem_total_bytes": 1000}},
            "tools": {"manifest": {
                "sqlite_version": "3.51.3", "sqlite_source_id": "source",
                "adaptation": {"patch_sha256": "b" * 64},
                "compile": {"cc": "cc test", "common_flags": "-O2"},
                "features": {"threadsafe": 1},
                "artifacts_sha256": {"sqlite3": "c" * 64}}}}}


def make_run(root, multiplier=1):
    root.mkdir()
    planned, summaries = [], []
    for case, profile in GROUPS:
        def samples(scale):
            values = [scale * repetition * multiplier for repetition in (1, 2, 3)]
            return {"samples": values, "median": values[1],
                    "range": {"minimum": values[0], "maximum": values[2]}}
        summaries.append({"case": case, "profile": profile, "sample_count": 3,
                          "expected_repetitions_complete": True,
                          "workload_ns": samples(100),
                          "workload_plus_drain_ns": samples(120),
                          "operations_per_second": ({"available": False}
                              if case == "speedtest-main" else samples(10.0)),
                          "tail_latency": {"available": False}})
        for repetition in (1, 2, 3):
            identity = f"{case}--{profile}--repetition-{repetition}"
            planned.append(identity)
            directory = root / identity
            directory.mkdir()
            (directory / "result.jsonl").write_text(
                json.dumps(result(case, profile, repetition, multiplier)) + "\n")
    manifest = {
        "schema": "block-storage-sqlite-suite-index", "schema_version": 1,
        "planned_full": 87, "selected": 87,
        "actual": {"attempted": 87, "completed": 87, "failed": 0},
        "planned_selections": planned, "completed_selections": planned,
        "failed_selections": []}
    summary = {
        "schema": "block-storage-sqlite-summary", "schema_version": 2,
        "scope": "ext4_matrix", "status": "pass", "selected": 87,
        "planned_full": 87, "expected_selections": planned,
        "manifest": "manifest.json", "samples": summaries}
    (root / "manifest.json").write_text(json.dumps(manifest))
    (root / "summary.json").write_text(json.dumps(summary))
    return root


def rewrite_results(root, change):
    for path in root.glob("*/result.jsonl"):
        value = json.loads(path.read_text())
        change(value)
        path.write_text(json.dumps(value) + "\n")


class BaselineTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.run = make_run(self.root / "run")
        (self.root / "pointers").mkdir()
        self.pointer = self.root / "pointers" / "baseline.json"

    def tearDown(self):
        self.temp.cleanup()

    def cli(self, *args, ok=True):
        process = subprocess.run([sys.executable, str(SCRIPT), *map(str, args)],
                                 text=True, capture_output=True, check=False)
        self.assertEqual(process.returncode == 0, ok, process.stdout + process.stderr)
        return json.loads(process.stdout)

    def test_select_and_compare_reports_samples_and_arithmetic(self):
        selected = self.cli("select", self.run, self.pointer)
        self.assertTrue(selected["baseline_pointer"]["selected_explicitly"])
        self.assertEqual(len(selected["baseline_pointer"]["artifacts"]["results"]), 87)
        candidate = make_run(self.root / "candidate", 2)
        compared = self.cli("compare", self.pointer, candidate)
        self.assertFalse(compared["self_comparison"])
        first = compared["groups"][0]["metrics"]["workload_ns"]
        self.assertEqual(first["baseline"]["samples"], [100, 200, 300])
        self.assertEqual(first["candidate"]["samples"], [200, 400, 600])
        self.assertEqual(first["median_percentage_change"], 100.0)
        speedtest = next(item for item in compared["groups"]
                         if item["group"].startswith("speedtest-main"))
        self.assertEqual(speedtest["metrics"]["operations_per_second"],
                         {"available": False, "reason": "unavailable in both runs"})
        self.assertFalse(compared["thresholds_applied"])

    def test_self_comparison_is_explicit_and_overwrite_is_rejected(self):
        self.cli("select", self.run, self.pointer)
        compared = self.cli("compare", self.pointer, self.run)
        self.assertTrue(compared["self_comparison"])
        self.assertFalse(compared["candidate_independent"])
        self.assertEqual(compared["groups"][0]["metrics"]["workload_ns"]
                         ["median_percentage_change"], 0.0)
        self.cli("select", self.run, self.pointer, ok=False)

    def test_tampering_and_incomplete_matrix_are_rejected(self):
        self.cli("select", self.run, self.pointer)
        artifact = next(self.run.glob("*/result.jsonl"))
        artifact.write_text(artifact.read_text() + "{}\n")
        self.cli("compare", self.pointer, self.run, ok=False)
        broken = make_run(self.root / "broken")
        manifest = json.loads((broken / "manifest.json").read_text())
        manifest["actual"]["failed"] = 1
        (broken / "manifest.json").write_text(json.dumps(manifest))
        self.cli("select", broken, self.root / "broken.json", ok=False)

    def test_hardware_and_commit_changes_require_correct_policy(self):
        self.cli("select", self.run, self.pointer)
        candidate = make_run(self.root / "candidate")
        rewrite_results(candidate, lambda value: value["metadata"]["tested_source"].update(
            {"commit": "b" * 40, "dirty": True}))
        self.cli("compare", self.pointer, candidate, ok=False)
        allowed = self.cli("compare", self.pointer, candidate, "--allow-commit-change")
        self.assertEqual(len(allowed["intentional_commit_changes"]), 29)
        self.assertTrue(allowed["intentional_commit_changes"][0]["candidate"]["dirty"])
        rewrite_results(candidate, lambda value: value["metadata"]["tested_source"]
                        ["hardware"].update({"online_cpus": 4}))
        self.cli("compare", self.pointer, candidate, "--allow-commit-change", ok=False)

    def test_invalid_numbers_duplicate_repetition_and_missing_validation_reject(self):
        for index, mutate in enumerate((
            lambda value: value["phases"]["workload_plus_drain"]["timings"].update(
                {"workload_ns": float("nan")}),
            lambda value: value["labels"].update({"repetition": 1}),
            lambda value: value.update({"validations": value["validations"][:1]}),
            lambda value: value["metadata"]["final_ledger"].update(
                {"write_reserved_bytes": 512 * 1024 * 1024 + 1}),
        )):
            run = make_run(self.root / f"invalid-{index}")
            path = list(run.glob("*/result.jsonl"))[1]
            value = json.loads(path.read_text())
            mutate(value)
            path.write_text(json.dumps(value) + "\n")
            self.cli("select", run, self.root / f"invalid-{index}.json", ok=False)


if __name__ == "__main__":
    unittest.main()
