"""Time the six-case portfolio in an owned directory on an existing ext4 mount."""

import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / "tools/build/go-ycsb-427715a/bin/go-ycsb"
LDB = ROOT / "tools/build/rocksdb-9.11.2/build/tools/ldb"
PROPERTIES = ROOT / "tools/go-ycsb/workloads"
EVIDENCE = ROOT / "evidence" / f"raw-ext4-timing-{time.time_ns()}"
EXPECTED_RECORDS = 8192
EXPECTED_REQUESTS = 131072


def capture(command):
    return subprocess.check_output(command, text=True, timeout=30).strip()


def run_phase(driver, profile, phase, database, records, requests, trace=False):
    name = f"{driver}-{profile}-{phase}" + ("-probe" if trace else "")
    args = [str(BINARY), phase, driver, "-P", str(PROPERTIES / "common.properties"),
            "-P", str(PROPERTIES / f"{profile}.properties"),
            "-p", f"recordcount={records}", "-p", f"operationcount={requests}",
            "-p", f"{driver}.{'db' if driver == 'sqlite' else 'dir'}={database}"]
    if driver == "rocksdb":
        args += ["-p", "rocksdb.write_buffer_size=16777216", "-p",
                 "rocksdb.max_write_buffer_number=2", "-p",
                 "rocksdb.cache_index_and_filter_blocks=true"]
    trace_path = EVIDENCE / f"{name}.strace"
    if trace:
        args = ["strace", "-f", "-qq", "-o", str(trace_path), "-e",
                "trace=fsync,fdatasync", *args]
    usage = EVIDENCE / f"{name}.usage.json"
    command = ["/usr/bin/time", "-o", str(usage), "-f",
               '{"wall_seconds":%e,"user_seconds":%U,"system_seconds":%S,"max_rss_kib":%M,"fs_inputs":%I,"fs_outputs":%O}', *args]
    start = time.monotonic()
    with (EVIDENCE / f"{name}.log").open("w") as output:
        subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, check=True, timeout=1800)
    elapsed = time.monotonic() - start
    output = (EVIDENCE / f"{name}.log").read_text()
    if re.search(r"(?im)\b(error|fatal|panic)\b", output):
        raise RuntimeError(f"operation error in {name}")
    counts = {key: int(value) for key, value in
              re.findall(r"(?m)^([A-Z_]+)\s+-.*?Count: (\d+)", output)}
    if phase == "load":
        assert counts["INSERT"] == counts["TOTAL"] == records, counts
    elif profile == "f":
        assert counts["READ"] == requests, counts
        assert counts["TOTAL"] == requests + counts["UPDATE"], counts
    else:
        assert counts["TOTAL"] == requests, counts
    report = {"command": command, "process_elapsed_seconds": elapsed,
              "usage": json.loads(usage.read_text()), "reported_counts": counts}
    if trace:
        calls = trace_path.read_text().splitlines()
        report["successful_sync_calls"] = sum(" = 0" in row for row in calls)
        assert report["successful_sync_calls"] >= records, report
    print(f"{name}: {elapsed:.3f}s", flush=True)
    return report


def verify(driver, database, expected):
    if driver == "sqlite":
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            assert connection.execute("PRAGMA integrity_check").fetchone() == ("ok",)
            row = connection.execute("SELECT count(*), min(length(field0)), max(length(field0)) FROM usertable").fetchone()
            assert row == (expected, 4096, 4096), row
        return {"rows": expected, "integrity": "ok", "value_bytes": 4096}
    output = capture([str(LDB), f"--db={database}", "dump", "--count_only"])
    assert re.search(rf"\b{expected}\b", output), output
    return {"row_count_output": output}


def main():
    drivers = sys.argv[1:] or ["sqlite", "rocksdb"]
    if len(set(drivers)) != len(drivers) or any(driver not in ("sqlite", "rocksdb") for driver in drivers):
        raise ValueError("optional arguments must be unique driver names: sqlite rocksdb")
    profiles = os.environ.get("MSFBLKDEV_TIMING_PROFILES", "a,f,e75").split(",")
    if len(set(profiles)) != len(profiles) or any(profile not in ("a", "f", "e75") for profile in profiles):
        raise ValueError("timing profiles must be unique names: a, f, e75")
    filesystem = capture(["findmnt", "-n", "-o", "FSTYPE", "-T", "/var/tmp"])
    if filesystem != "ext4":
        raise RuntimeError(f"/var/tmp is {filesystem}, expected ext4")
    EVIDENCE.mkdir()
    cgroup = Path("/sys/fs/cgroup") / Path("/proc/self/cgroup").read_text().strip().split("::", 1)[1].lstrip("/")
    report = {"scope": "duration calibration; existing host ext4; not ADR acceptance or final I/O-limited comparison",
              "filesystem": capture(["findmnt", "-n", "-o", "TARGET,SOURCE,FSTYPE,OPTIONS", "-T", "/var/tmp"]),
              "kernel": capture(["uname", "-r"]), "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
              "recordcount": EXPECTED_RECORDS, "operationcount": EXPECTED_REQUESTS,
              "field_bytes": 4096, "threads": 1, "seed": 123,
              "memory_max": (cgroup / "memory.max").read_text().strip(),
              "swap_max": (cgroup / "memory.swap.max").read_text().strip(),
              "sync_probes": [], "cases": []}
    assert report["memory_max"] == "268435456" and report["swap_max"] == "0", report
    (EVIDENCE / "build-manifest.txt").write_bytes((BINARY.parents[1] / "build-manifest.txt").read_bytes())
    start = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="msfblkdev-raw-ext4-", dir="/var/tmp") as directory:
        report["fixture"] = directory
        for driver in drivers:
            probe = Path(directory) / f"{driver}-sync-probe"
            report["sync_probes"].append(run_phase(driver, "a", "load", probe, 64, 128, trace=True))
            verify(driver, probe, 64)
        for driver in drivers:
            for profile in profiles:
                database = Path(directory) / f"{driver}-{profile}"
                case = {"driver": driver, "workload": profile}
                for phase in ("load", "run"):
                    case[phase] = run_phase(driver, profile, phase, database, EXPECTED_RECORDS, EXPECTED_REQUESTS)
                expected = EXPECTED_RECORDS + case["run"]["reported_counts"].get("INSERT", 0)
                case["verification"] = verify(driver, database, expected)
                report["cases"].append(case)
                (EVIDENCE / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        report["cgroup_memory_peak"] = (cgroup / "memory.peak").read_text().strip()
        report["cgroup_memory_events"] = (cgroup / "memory.events").read_text()
    report["fixture_removed"] = not Path(directory).exists()
    report["total_elapsed_seconds"] = time.monotonic() - start
    report["measured_phase_seconds"] = sum(case[phase]["process_elapsed_seconds"] for case in report["cases"] for phase in ("load", "run"))
    report["status"] = "pass"
    (EVIDENCE / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"report: {EVIDENCE / 'report.json'}", flush=True)


if __name__ == "__main__":
    main()
