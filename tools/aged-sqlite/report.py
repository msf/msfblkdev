#!/usr/bin/env python3
"""Writes report.html for an aged SQLite matrix directory: tables and one bar chart per measure.

Usage: report.py <matrix-dir>

A run's value for each measure comes from its last two windows, which the stop rule judged steady
or which ended the run. A cell shows the median over the passing repeats and the min-max range.
"""

from __future__ import annotations

import html
import json
import statistics
import sys
from dataclasses import dataclass
from pathlib import Path

STEADY_WINDOWS = 2
# Matrices from before the client dimension: SQLite YCSB only, named variant-workload-rN.
LEGACY_WRITE_OPERATION = {"a": "UPDATE", "f": "READ_MODIFY_WRITE"}
LEGACY_BENCHMARKS = {"a": "sqlite-a", "f": "sqlite-f"}
COLORS = ["#4269d0", "#efb118", "#ff725c", "#6cc5b0", "#3ca951", "#ff8ab7", "#a463f2", "#97bbf5"]


@dataclass(frozen=True)
class Measure:
    key: str
    title: str
    unit: str
    better: str  # "higher" or "lower"


MEASURES = [
    Measure("tps", "Throughput", "units/s", "higher"),
    Measure("write_p50", "Write latency p50", "µs", "lower"),
    Measure("write_p99", "Write latency p99", "µs", "lower"),
    Measure("write_p999", "Write latency p99.9", "µs", "lower"),
    Measure("read_p99", "Read latency p99", "µs", "lower"),
    Measure("write_amplification", "Write amplification (medium / device)", "×", "lower"),
    Measure("daemon_cpu_us", "Daemon CPU per operation", "µs", "lower"),
    Measure("client_cpu_us", "Client CPU per operation", "µs", "lower"),
]


def run_measures(summary: dict) -> dict[str, float | None]:
    """The steady-state value of every measure for one passing run."""
    write = summary["facts"].get("write_operation") or LEGACY_WRITE_OPERATION[summary["config"]["workload"]]
    windows = [phase for phase in summary["phases"] if phase["name"].startswith("window-")]
    steady = windows[-STEADY_WINDOWS:]
    if not steady:
        return {}

    def mean(values: list[float]) -> float | None:
        values = [value for value in values if value is not None]
        return statistics.fmean(values) if values else None

    operations = sum(phase["operations"] for phase in steady)

    def cpu_per_operation(key: str) -> float | None:
        seconds = [phase.get(key) for phase in steady]
        return None if None in seconds else sum(seconds) / operations * 1e6

    return {
        "tps": mean([phase["operations_per_second"] for phase in steady]),
        "write_p50": mean([phase["latency"].get(write, {}).get("p50") for phase in steady]),
        "write_p99": mean([phase["latency"].get(write, {}).get("p99") for phase in steady]),
        "write_p999": mean([phase["latency"].get(write, {}).get("p999") for phase in steady]),
        "read_p99": mean([phase["latency"].get("READ", {}).get("p99") for phase in steady]),
        "write_amplification": mean([phase["write_amplification"] for phase in steady]),
        "daemon_cpu_us": cpu_per_operation("daemon_cpu_seconds"),
        "client_cpu_us": cpu_per_operation("client_cpu_seconds"),
    }


@dataclass
class Cell:
    median: float
    low: float
    high: float
    count: int


def aggregate(values: list[float | None]) -> Cell | None:
    present = [value for value in values if value is not None]
    if not present:
        return None
    return Cell(statistics.median(present), min(present), max(present), len(present))


def parse_name(name: str) -> tuple[str, str]:
    if "__" in name:
        variant, benchmark, _ = name.split("__")
        return variant, benchmark
    variant, workload, _ = name.rsplit("-", 2)
    return variant, LEGACY_BENCHMARKS[workload]


def load_plan(matrix: Path) -> dict:
    plan = json.loads((matrix / "matrix.json").read_text())
    if "benchmarks" not in plan:
        plan["benchmarks"] = {LEGACY_BENCHMARKS[w]: f"YCSB {w.upper()} on SQLite" for w in plan["workloads"]}
    return plan


def collect(matrix: Path) -> tuple[dict, dict, list[dict]]:
    plan = load_plan(matrix)
    per_cell: dict[tuple[str, str], list[dict[str, float | None]]] = {}
    runs = []
    for name in plan["order"]:
        variant, workload = parse_name(name)
        summary_path = matrix / "runs" / name / "summary.json"
        summary = json.loads(summary_path.read_text()) if summary_path.exists() else None
        result = summary["result"] if summary else "NOT RUN"
        runs.append({"name": name, "variant": variant, "workload": workload, "result": result,
                     "stop": summary["facts"].get("stop_reason") if summary else None,
                     "summary": summary})
        if result == "PASS":
            per_cell.setdefault((variant, workload), []).append(run_measures(summary))
    cells = {key: {measure.key: aggregate([run.get(measure.key) for run in values]) for measure in MEASURES}
             for key, values in per_cell.items()}
    return plan, cells, runs


def number(value: float) -> str:
    if value >= 100:
        return f"{value:,.0f}"
    if value >= 10:
        return f"{value:.1f}"
    return f"{value:.2f}"


def bar_chart(measure: Measure, benchmark: str, plan: dict, cells: dict) -> str:
    """One SVG for one benchmark: a bar per variant with a whisker from min to max."""
    variants = [v for v in plan["variants"] if cells.get((v, benchmark), {}).get(measure.key)]
    if not variants:
        return ""
    width, height, left, bottom, top = 520, 280, 70, 20, 30
    top_value = max(cells[(v, benchmark)][measure.key].high for v in variants) * 1.15 or 1
    slot = (width - left - 10) / len(plan["variants"])
    bar_width = min(56, slot * 0.7)
    plot_height = height - bottom - top
    scale = plot_height / top_value
    parts = [f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="{html.escape(measure.title)}">',
             f'<text x="{left}" y="18" class="title">{html.escape(measure.title)} ({measure.unit}, '
             f'{measure.better} is better)</text>']
    for tick in range(5):
        value = top_value * tick / 4
        y = top + plot_height * (1 - tick / 4)
        parts.append(f'<line x1="{left}" x2="{width - 10}" y1="{y:.1f}" y2="{y:.1f}" class="grid"/>'
                     f'<text x="{left - 6}" y="{y + 4:.1f}" class="tick" text-anchor="end">{number(value)}</text>')
    for index, variant in enumerate(plan["variants"]):
        cell = cells.get((variant, benchmark), {}).get(measure.key)
        if not cell:
            continue
        x = left + index * slot + (slot - bar_width) / 2
        y, low, high = (top + plot_height - v * scale for v in (cell.median, cell.low, cell.high))
        label = f"{plan['variants'][variant]}: {number(cell.median)} {measure.unit} (n={cell.count})"
        parts.append(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{bar_width:.1f}" height="{top + plot_height - y:.1f}" '
            f'fill="{COLORS[index % len(COLORS)]}"><title>{html.escape(label)}</title></rect>'
            f'<line x1="{x + bar_width / 2:.1f}" x2="{x + bar_width / 2:.1f}" y1="{low:.1f}" y2="{high:.1f}" '
            f'class="whisker"/>'
            f'<text x="{x + bar_width / 2:.1f}" y="{min(y, high) - 4:.1f}" class="value" '
            f'text-anchor="middle">{number(cell.median)}</text>')
    parts.append("</svg>")
    return "".join(parts)


def legend(plan: dict) -> str:
    items = "".join(f'<span><i style="background:{COLORS[i % len(COLORS)]}"></i>{html.escape(label)}</span>'
                    for i, label in enumerate(plan["variants"].values()))
    return f'<div class="legend">{items}</div>'


def section(benchmark: str, plan: dict, cells: dict, runs: list[dict]) -> str:
    head = "".join(f"<th>{html.escape(m.title)}<br><small>{m.unit}</small></th>" for m in MEASURES)
    rows = []
    for variant, label in plan["variants"].items():
        total = sum(1 for run in runs if run["variant"] == variant and run["workload"] == benchmark)
        if not total:
            continue
        row = cells.get((variant, benchmark), {})
        passed = next((cell.count for cell in row.values() if cell), 0)
        values = "".join(
            f"<td>{number(cell.median)}<br><small>{number(cell.low)}–{number(cell.high)}</small></td>"
            if (cell := row.get(m.key)) else "<td>–</td>" for m in MEASURES)
        rows.append(f"<tr><th>{html.escape(label)}</th><td>{passed}/{total}</td>{values}</tr>")
    charts = "".join(f"<div>{chart}</div>" for m in MEASURES if (chart := bar_chart(m, benchmark, plan, cells)))
    return (f"<h2>{html.escape(plan['benchmarks'][benchmark])}</h2>"
            f"<table><tr><th>Backing</th><th>Passed</th>{head}</tr>" + "".join(rows) + "</table>"
            f"{legend(plan)}<div class=charts>{charts}</div>")


def environment(runs: list[dict]) -> str:
    for run in runs:
        host = run["summary"] and run["summary"]["facts"].get("environment_start")
        if host:
            fields = {key: host.get(key) for key in ("commit", "kernel", "cpu", "governors", "boost",
                                                     "daemon_cpus", "client_cpus")}
            return "<p class=env>" + " · ".join(f"<b>{k}</b> {html.escape(str(v))}" for k, v in fields.items()) + "</p>"
    return ""


def run_list(runs: list[dict]) -> str:
    rows = "".join(
        f'<tr><td>{i}</td><td><a href="runs/{run["name"]}/summary.md">{run["name"]}</a></td>'
        f'<td class="{run["result"].replace(" ", "-")}">{run["result"]}</td><td>{run["stop"] or ""}</td></tr>'
        for i, run in enumerate(runs, 1))
    head = "<tr><th>#</th><th>Run</th><th>Result</th><th>Stop</th></tr>"
    return f"<h2>Runs in execution order</h2><table>{head}{rows}</table>"


STYLE = """
body{font:14px/1.4 system-ui,sans-serif;margin:24px;color:#222;max-width:1500px}
table{border-collapse:collapse;margin:8px 0 24px}th,td{border:1px solid #ddd;padding:6px 10px;text-align:right}
th:first-child{text-align:left}small{color:#777}.env{color:#555}
.charts{display:grid;grid-template-columns:repeat(auto-fill,minmax(520px,1fr));gap:16px}
svg{width:100%;background:#fafafa;border:1px solid #eee}.title{font-weight:600;font-size:13px}
.grid{stroke:#e5e5e5}.tick,.axis{font-size:11px;fill:#555}.value{font-size:11px;fill:#222}
.whisker{stroke:#222;stroke-width:1.5}.legend span{margin-right:18px}
.legend i{display:inline-block;width:12px;height:12px;margin-right:6px;vertical-align:-1px}
.PASS{color:#1a7f37}.FAIL{color:#cf222e}.NOT-RUN{color:#777}
"""


def write(matrix: Path) -> Path:
    plan, cells, runs = collect(matrix)
    page = (f"<!doctype html><meta charset=utf-8><title>Aged workload matrix</title><style>{STYLE}</style>"
            f"<h1>Aged workload matrix</h1>{environment(runs)}"
            f"<p>Each cell: median of passing repeats, min–max below. Measures come from the last "
            f"{STEADY_WINDOWS} steady windows of each run. A unit is a YCSB operation or a 4 KiB "
            f"fio write. Write latency is UPDATE (YCSB A), READ_MODIFY_WRITE (YCSB F) or the write "
            f"completion (fio).</p>"
            + "".join(section(b, plan, cells, runs) for b in plan["benchmarks"]) + run_list(runs))
    target = matrix / "report.html"
    target.write_text(page)
    return target


if __name__ == "__main__":
    print(write(Path(sys.argv[1])))
