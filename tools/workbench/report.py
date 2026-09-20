"""Render a compact view while retaining every normalized sample."""

import csv
import json
import statistics
from pathlib import Path

FIELDS = (
    "run",
    "database",
    "filesystem",
    "case",
    "profile",
    "repetition",
    "status",
    "commit",
    "dirty",
    "load_s",
    "workload_s",
    "workload_drain_s",
    "operations",
    "ops_s",
    "backing_read_mib",
    "backing_write_mib",
    "backing_requests",
    "backing_flushes",
    "peak_rss_mib",
    "p99_api_us",
    "p9999_api_us",
    "physical_used_mib",
    "source",
)


def cell(value):
    if value is None:
        return "N/A"
    if isinstance(value, float):
        return f"{value:,.3f}"
    return str(value).replace("|", "\\|").replace("\n", " ")


def table(headers, rows):
    return [
        "| " + " | ".join(map(cell, headers)) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
        *["| " + " | ".join(map(cell, row)) + " |" for row in rows],
        "",
    ]


def groups(rows):
    result = {}
    for row in rows:
        result.setdefault(
            (row["database"], row["filesystem"], row["case"], row["profile"]), []
        ).append(row)
    return result


def median(rows, field):
    values = [
        row[field]
        for row in rows
        if row["status"] in ("pass", "calibration") and row[field] is not None
    ]
    return statistics.median(values) if len(values) == len(rows) and values else None


def run_section(run):
    lines = [
        f"## {run['id']}",
        "",
        f"{run['kind']}: **{run['status']}**. Source: `{run['source']}`.",
        "",
    ]
    lines.extend(f"- {cell(issue)}" for issue in run["issues"])
    lines += [
        "",
        "Values are per-case medians across the listed repetitions; single samples remain single samples.",
        "",
    ]
    data = []
    for (database, filesystem, case, profile), rows in groups(run["rows"]).items():
        passed = sum(row["status"] in ("pass", "calibration") for row in rows)
        data.append(
            [
                database,
                filesystem,
                case,
                profile,
                f"{passed}/{len(rows)}",
                *[
                    median(rows, name)
                    for name in (
                        "workload_s",
                        "ops_s",
                        "backing_read_mib",
                        "backing_write_mib",
                    )
                ],
            ]
        )
    lines += table(
        [
            "DB",
            "FS",
            "Case",
            "Profile",
            "Samples OK",
            "Workload s",
            "Ops/s",
            "Backing read MiB",
            "Backing written MiB",
        ],
        data,
    )
    timing_rows = [row for row in run["rows"] if row["api_latencies"]]
    if timing_rows:
        data = [
            [
                row["database"],
                row["case"],
                operation,
                tail["count"],
                tail["p99_us"],
                tail["p9999_us"],
                tail["upper_tail_samples"],
            ]
            for row in timing_rows
            for operation, tail in row["api_latencies"].items()
        ]
        lines += [
            "Final cumulative API-call latency summaries, not averaged interval percentiles. Small tail counts are weak evidence.",
            "",
        ]
        lines += table(
            [
                "DB",
                "Case",
                "API operation",
                "Samples",
                "p99 µs",
                "p99.99 µs",
                "Samples above p99.99 ≈",
            ],
            data,
        )
    for row in run["rows"]:
        lines.extend(f"- {cell(row['case'])}: {cell(issue)}" for issue in row["issues"])
    return lines + [""]


def comparison(runs):
    columns = []
    keys = set()
    for run in runs:
        grouped = {}
        for row in run["rows"]:
            key = (row["database"] or "unknown", row["case"], row["profile"])
            grouped.setdefault(key, []).append(row)
            keys.add(key)
        columns.append(grouped)
    rows = []
    for key in sorted(keys):
        values = [median(column.get(key, []), "workload_s") for column in columns]
        rows.append([" / ".join(key), *values])
    return [
        "## Workload seconds by case",
        "",
        "Columns follow the input-run order above. Medians only; configuration differences remain visible in the per-run sections and JSON.",
        "N/A can also mean that this case was not selected in that input. No percentage-change or regression verdict is inferred.",
        "",
        *table(
            ["DB / case / profile", *[f"Run {i + 1}" for i in range(len(runs))]], rows
        ),
    ]


def write_report(runs, output):
    rows = [row for run in runs for row in run["rows"]]
    if not rows:
        raise ValueError("selection contains no case results")
    output = Path(output)
    output.mkdir(parents=True, exist_ok=False)
    document = {
        "schema": "workbench-report",
        "schema_version": 1,
        "runs": runs,
        "comparison": "descriptive only; no automatic regression or equivalence verdict",
        "units": {"times": "seconds", "latency": "microseconds", "sizes": "MiB"},
    }
    (output / "report.json").write_text(
        json.dumps(document, indent=2, allow_nan=False) + "\n"
    )
    with (output / "cases.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=FIELDS, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)
    lines = [
        "# Workbench evidence report",
        "",
        "Descriptive comparison of selected evidence. Correctness and calibration are separate result classes.",
        "",
        "N/A means unavailable, never zero. CSV blanks and JSON nulls have the same meaning.",
        "Backing I/O covers the workload plus drain, excludes initialization and validation, and is not occupied physical space.",
        "YCSB operation rates use logical requests; F's API-call latency counts include its additional updates.",
        "The CSV retains each repetition, load/drain times, request/flush counts and peak RSS where measured.",
        "The JSON retains source paths, configuration, all API latency summaries and failures. No raw evidence is changed.",
        "",
    ]
    lines += table(
        ["Column", "Run", "FS", "Kind", "Status", "Selected samples"],
        [
            [
                f"Run {index + 1}",
                run["id"],
                ", ".join(
                    sorted({row["filesystem"] or "unknown" for row in run["rows"]})
                ),
                run["kind"],
                run["status"],
                len(run["rows"]),
            ]
            for index, run in enumerate(runs)
        ],
    )
    if len(runs) > 1:
        lines += comparison(runs)
    for run in runs:
        lines += run_section(run)
    (output / "report.md").write_text("\n".join(lines))
    return output / "report.md"
