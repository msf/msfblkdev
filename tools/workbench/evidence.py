"""Read existing evidence without upgrading calibration into acceptance."""

import math
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "sqlite"))
import baseline


def number(value):
    if value is None or isinstance(value, dict):
        return None
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value < 0
    ):
        raise ValueError(f"invalid metric: {value!r}")
    return value


def scaled(value, divisor):
    value = number(value)
    return None if value is None else value / divisor


def empty_row(run, case, profile, repetition, source):
    return {
        "run": run,
        "case": case,
        "profile": profile,
        "repetition": repetition,
        "source": str(source),
        "status": "missing",
        "database": None,
        "filesystem": None,
        "commit": None,
        "dirty": None,
        "load_s": None,
        "workload_s": None,
        "workload_drain_s": None,
        "operations": None,
        "ops_s": None,
        "backing_read_mib": None,
        "backing_write_mib": None,
        "backing_requests": None,
        "backing_flushes": None,
        "peak_rss_mib": None,
        "p99_api_us": None,
        "p9999_api_us": None,
        "physical_used_mib": None,
        "api_latencies": {},
        "parameters": {},
        "issues": [],
    }


def sqlite_row(root, identity, scope):
    case, profile, repetition = identity.split("--")
    path = root / identity / "result.jsonl"
    row = empty_row(
        root.name, case, profile, int(repetition.removeprefix("repetition-")), path
    )
    row.update(database="sqlite", filesystem=baseline.FILESYSTEMS[scope])
    if not path.exists():
        row["issues"].append("Selected case has no result file.")
        return row
    record = baseline.read_json(path, baseline.MAX_RESULT, True)
    row["status"] = record.get("status", "invalid")
    row["issues"] = record.get("errors", [])
    if row["status"] == "pass":
        baseline.validate_result(record, identity, scope)
    source = record.get("metadata", {}).get("tested_source", {})
    row.update(
        commit=source.get("commit"),
        dirty=source.get("dirty"),
        parameters=record.get("parameters", {}),
        cache=record.get("cache_condition"),
        configuration=source,
    )
    phases = record.get("phases", {})
    workload = phases.get("workload_plus_drain", {})
    timings = workload.get("timings", {})
    row.update(
        load_s=scaled(phases.get("initialization", {}).get("duration_ns"), 1e9),
        workload_s=scaled(timings.get("workload_ns"), 1e9),
        workload_drain_s=scaled(timings.get("workload_plus_drain_ns"), 1e9),
        operations=number(workload.get("completed_operations")),
        ops_s=number(workload.get("operations_per_second")),
    )
    segments = workload.get("io_segments", {})
    names = (
        "before_tool_to_drain_start_workload",
        "drain_start_to_drain_end",
        "drain_end_to_parent_after_residual",
    )
    if segments.get("segments_are_non_overlapping") is True:
        totals = {
            key: sum(number(segments[name][key]) for name in names)
            for key in (
                "backing_read_completed_bytes",
                "backing_write_completed_bytes",
                "backing_completed_reads",
                "backing_completed_writes",
                "backing_completed_flushes",
            )
        }
        row.update(
            backing_read_mib=scaled(totals["backing_read_completed_bytes"], 2**20),
            backing_write_mib=scaled(totals["backing_write_completed_bytes"], 2**20),
            backing_requests=totals["backing_completed_reads"]
            + totals["backing_completed_writes"],
            backing_flushes=totals["backing_completed_flushes"],
        )
    return row


def read_sqlite(root):
    manifest = baseline.read_json(root / "manifest.json")
    if (manifest.get("schema"), manifest.get("schema_version")) != (
        "block-storage-sqlite-suite-index",
        1,
    ):
        raise ValueError("unsupported SQLite manifest")
    selections = manifest.get("planned_selections", [])
    if (
        not selections
        or len(selections) > baseline.RUNS
        or len(set(selections)) != len(selections)
    ):
        raise ValueError("invalid SQLite selection count")
    if any(identity not in baseline.catalog() for identity in selections):
        raise ValueError("unknown SQLite selection")
    summary = (
        baseline.read_json(root / "summary.json")
        if (root / "summary.json").exists()
        else {}
    )
    filesystem = manifest.get(
        "filesystem", baseline.FILESYSTEMS.get(summary.get("scope"))
    )
    scopes = {value: key for key, value in baseline.FILESYSTEMS.items()}
    if filesystem not in scopes:
        raise ValueError("unknown SQLite filesystem")
    run = {
        "id": root.name,
        "source": str(root),
        "kind": "sqlite-correctness",
        "status": manifest.get("status", "incomplete"),
        "issues": [],
        "rows": [],
    }
    if run["status"] == "pass" and (
        manifest.get("completed_selections") != selections
        or manifest.get("selected") != len(selections)
        or manifest.get("actual")
        != {"attempted": len(selections), "completed": len(selections), "failed": 0}
        or summary.get("status") != "pass"
        or summary.get("scope") != scopes[filesystem]
    ):
        run.update(
            status="invalid",
            issues=["Manifest completion or summary disagrees with the selection."],
        )
    for identity in selections:
        try:
            run["rows"].append(sqlite_row(root, identity, scopes[filesystem]))
        except (ValueError, KeyError, TypeError) as error:
            row = empty_row(root.name, identity, "unknown", 0, root / identity)
            row.update(status="invalid", issues=[str(error)])
            run["rows"].append(row)
    if run["status"] == "pass" and any(row["status"] != "pass" for row in run["rows"]):
        run["status"] = "invalid"
    if run["status"] == "pass" and len(selections) == baseline.RUNS:
        try:
            baseline.validate_run(root)
            run["status"] = "validated-full-matrix"
        except (ValueError, KeyError, TypeError) as error:
            run.update(status="invalid", issues=[str(error)])
    run["issues"].append(
        "Warm-cache correctness timings; backing counters exclude host-device amplification. Physical occupancy and latency tails unavailable."
    )
    return run


def latencies(path, counts):
    if not path.exists():
        return {}
    if path.stat().st_size > baseline.MAX_JSON:
        raise ValueError(f"oversized YCSB log: {path}")
    result = {}
    text = path.read_text()
    if "Run finished," not in text:
        return {}
    for line in text.rsplit("Run finished,", 1)[1].splitlines():
        match = re.match(
            r"^([A-Z_]+)\s+- .*Count: (\d+),.*99th\(us\): (\d+),.*99\.99th\(us\): (\d+)",
            line,
        )
        if match:
            operation, count, p99, p9999 = match.groups()
            if int(count) != counts.get(operation):
                raise ValueError(f"latency count mismatch: {operation}")
            result[operation] = {
                "count": int(count),
                "p99_us": int(p99),
                "p9999_us": int(p9999),
                "upper_tail_samples": int(count) / 10000,
            }
    return result


def timing_row(root, report, case):
    driver, profile = case["driver"], case["workload"]
    if driver not in ("sqlite", "rocksdb") or profile not in (
        "a",
        "f",
        "e75",
        "e95",
        "e50",
    ):
        raise ValueError("unknown timing case")
    if not case.get("verification"):
        raise ValueError("timing case has no validation evidence")
    row = empty_row(root.name, profile, "sync-writes", 1, root / "report.json")
    run, load = case["run"], case["load"]
    counts = run["reported_counts"]
    requests = report["operationcount"]
    actual = counts["READ"] if profile == "f" else counts["TOTAL"]
    if actual != requests or load["reported_counts"]["INSERT"] != report["recordcount"]:
        raise ValueError("YCSB operation counts differ from requested counts")
    if profile == "f" and counts["TOTAL"] != actual + counts["UPDATE"]:
        raise ValueError("YCSB F API-call count differs from reads plus updates")
    elapsed = number(run["process_elapsed_seconds"])
    if not elapsed:
        raise ValueError("YCSB elapsed time must be positive")
    tails = latencies(root / f"{driver}-{profile}-run.log", counts)
    total = tails.get("TOTAL", {})
    row.update(
        status="calibration",
        database=driver,
        filesystem="ext4-host",
        load_s=number(load["process_elapsed_seconds"]),
        workload_s=elapsed,
        operations=actual,
        ops_s=actual / elapsed,
        peak_rss_mib=scaled(run["usage"].get("max_rss_kib"), 1024),
        p99_api_us=total.get("p99_us"),
        p9999_api_us=total.get("p9999_us"),
        api_latencies=tails,
        cache="cached host-ext4 timing calibration",
        parameters={
            key: report[key]
            for key in (
                "recordcount",
                "operationcount",
                "field_bytes",
                "threads",
                "seed",
            )
        },
        configuration={
            key: report.get(key)
            for key in (
                "filesystem",
                "kernel",
                "binary_sha256",
                "memory_max",
                "swap_max",
            )
        },
    )
    return row


def read_timing(root):
    report = baseline.read_json(root / "report.json")
    if "duration calibration" not in report.get("scope", ""):
        raise ValueError("unsupported timing report scope")
    cases = report.get("cases", [])
    if not isinstance(cases, list) or len(cases) > 100:
        raise ValueError("invalid timing case count")
    run = {
        "id": root.name,
        "source": str(root),
        "kind": "ycsb-host-calibration",
        "status": "calibration" if report.get("status") == "pass" else "incomplete",
        "issues": [
            "Cached host ext4, 512-byte sectors; not a VM or storage comparison. Throughput uses process elapsed time, including open/close. Device I/O and occupied space unavailable."
        ],
        "rows": [],
    }
    identities = [(case.get("driver"), case.get("workload")) for case in cases]
    if len(set(identities)) != len(identities):
        raise ValueError("duplicate timing case")
    for case in cases:
        try:
            run["rows"].append(timing_row(root, report, case))
        except (ValueError, KeyError, TypeError) as error:
            row = empty_row(
                root.name,
                case.get("workload", "unknown"),
                "sync-writes",
                1,
                root / "report.json",
            )
            row.update(
                status="invalid", database=case.get("driver"), issues=[str(error)]
            )
            run["rows"].append(row)
            run["status"] = "invalid"
    if report.get("fixture_removed") is not True:
        run["issues"].append("Fixture cleanup is not confirmed.")
    return run


def read_run(path):
    root = Path(path).resolve()
    if root.is_file():
        root = root.parent
    if (root / "manifest.json").exists():
        return read_sqlite(root)
    if (root / "report.json").exists():
        return read_timing(root)
    raise ValueError(f"no supported manifest.json or report.json in {root}")
