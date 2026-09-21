#!/usr/bin/env python3
"""Turn adr08-bench JSONL into a CSV and a Markdown report with per-job medians."""
import argparse
import csv
import json
from pathlib import Path
from statistics import median

FIELDS = [
    "main_completed", "main_enospc", "main_elapsed_ns", "main_ops_per_s", "main_op_p50_ns",
    "main_op_p99_ns", "main_op_p999_ns", "main_op_max_ns", "main_flush_p99_ns", "main_ticks",
    "main_tick_p99_ns", "fill_completed", "fill_ops_per_s", "rss_bytes", "reopen_ns", "reopen_rss_bytes",
    "open_ns", "rss_after_open_bytes", "rss_after_writes_bytes", "stat_heap_total_bytes",
    "stat_heap_allocated_bytes", "stat_cache_bytes", "stat_index_bytes", "stat_reclaim_payloads_copied",
    "stat_reclaim_segments_freed", "stat_reclaim_pressure_steps", "stat_reclaim_pressure_waits",
    "stat_reclaim_enospc_all_live", "stat_reclaim_barrier_publications", "stat_log_relocation_payload_blocks",
    "stat_log_user_payload_blocks", "stat_free_segments", "stat_segment_count", "allocated_bytes",
    "stat_live_bytes", "stat_log_used_bytes", "stat_log_capacity_bytes", "total_run_seconds", "exit_code",
]


def load(path):
    metadata = None
    records = []
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        item = json.loads(line)
        if "metadata" in item:
            metadata = item["metadata"]
        else:
            records.append(item)
    return metadata, records


def group(records):
    groups = {}
    for record in records:
        groups.setdefault((record["comparison"], record["label"], record["version"]), []).append(record)
    return groups


def med(values):
    values = [v for v in values if isinstance(v, (int, float))]
    return median(values) if values else None


def fmt(value, unit=""):
    if value is None:
        return "—"
    if unit == "ms":
        return f"{value / 1e6:.2f} ms"
    if unit == "us":
        return f"{value / 1e3:.0f} µs"
    if unit == "MiB":
        return f"{value / (1 << 20):.1f} MiB"
    if isinstance(value, float):
        return f"{value:,.0f}"
    return f"{value:,}"


def rows(groups, comparison, columns):
    out = []
    for (name, label, version), runs in sorted(groups.items()):
        if name != comparison:
            continue
        ok = [r for r in runs if r.get("exit_code") == 0]
        cells = [label, version, f"{len(ok)}/{len(runs)}"]
        for key, unit in columns:
            cells.append(fmt(med([r.get(key) for r in ok]), unit))
        out.append(cells)
    return out


def table(header, body):
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in body]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--csv", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    metadata, records = load(args.input)
    with args.csv.open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(["comparison", "label", "version", "repetition"] + FIELDS)
        for record in records:
            writer.writerow([record.get("comparison"), record.get("label"), record.get("version"),
                             record.get("repetition")] + [record.get(field, "") for field in FIELDS])
    groups = group(records)
    parts = [
        "# ADR-08 performance comparisons",
        "",
        f"Baseline `{metadata['baseline_commit'][:7]}` (format 3) against candidate "
        f"`{metadata['candidate_commit'][:7]}` (format 4). Kernel `{metadata['kernel']}`, CPU {metadata['cpu']}, "
        f"{metadata['repetitions']} interleaved repetitions, `{metadata['rustc']}`, fixture filesystem "
        f"`{metadata['filesystem']}`. Values are medians over successful repetitions; `runs` counts successful/total.",
        "",
        "## 1. Fresh volume, nothing to reclaim",
        "",
        table(["job", "version", "runs", "ops/s", "op p50", "op p99", "op p99.9", "flush p99", "RSS"],
              rows(groups, "fresh", [("main_ops_per_s", ""), ("main_op_p50_ns", "us"), ("main_op_p99_ns", "us"),
                                     ("main_op_p999_ns", "us"), ("main_flush_p99_ns", "ms"), ("rss_bytes", "MiB")])),
        "",
        "## 2. Aged overwrite for twice the log capacity",
        "",
        table(["job", "version", "runs", "completed", "ENOSPC", "ops/s", "op p99", "op p99.9", "op max",
               "copies", "freed", "pressure steps", "reopen"],
              rows(groups, "aged", [("main_completed", ""), ("main_enospc", ""), ("main_ops_per_s", ""),
                                    ("main_op_p99_ns", "us"), ("main_op_p999_ns", "us"), ("main_op_max_ns", "ms"),
                                    ("stat_reclaim_payloads_copied", ""), ("stat_reclaim_segments_freed", ""),
                                    ("stat_reclaim_pressure_steps", ""), ("reopen_ns", "ms")])),
        "",
        "## 3. Progress and latency: scheduled ticks against inline pressure only",
        "",
        table(["job", "version", "runs", "completed", "ops/s", "op p99", "op p99.9", "op max", "ticks",
               "tick p99", "pressure steps", "pressure waits", "copies"],
              rows(groups, "progress", [("main_completed", ""), ("main_ops_per_s", ""), ("main_op_p99_ns", "us"),
                                        ("main_op_p999_ns", "us"), ("main_op_max_ns", "ms"), ("main_ticks", ""),
                                        ("main_tick_p99_ns", "us"), ("stat_reclaim_pressure_steps", ""),
                                        ("stat_reclaim_pressure_waits", ""), ("stat_reclaim_payloads_copied", "")])),
        "",
        "## 4. Memory and restart",
        "",
        table(["job", "version", "runs", "open", "RSS after open", "RSS after writes", "reopen", "RSS after reopen",
               "heap total", "heap allocated", "cache", "index"],
              rows(groups, "memory", [("open_ns", "ms"), ("rss_after_open_bytes", "MiB"),
                                      ("rss_after_writes_bytes", "MiB"), ("reopen_ns", "ms"),
                                      ("reopen_rss_bytes", "MiB"), ("stat_heap_total_bytes", "MiB"),
                                      ("stat_heap_allocated_bytes", "MiB"), ("stat_cache_bytes", "MiB"),
                                      ("stat_index_bytes", "MiB")])),
        "",
    ]
    failures = [r for r in records if r.get("exit_code") != 0]
    if failures:
        parts += ["## Failed runs", ""]
        parts += [f"- {r['comparison']}/{r['label']} {r['version']} repetition {r['repetition']}: "
                  f"exit {r['exit_code']}: `{r.get('stderr', '')[-300:]}`" for r in failures]
        parts.append("")
    args.report.write_text("\n".join(parts))
    print(f"wrote {args.csv} and {args.report}")


if __name__ == "__main__":
    main()
