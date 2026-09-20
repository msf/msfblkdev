"""Inspect the planned matrix and summarize existing evidence."""

import argparse
import fnmatch
import itertools
import sys
from pathlib import Path

from report import table, write_report

from evidence import baseline, read_run


def matrix():
    config = baseline.read_json(Path(__file__).with_name("matrix.json"))
    columns = list(itertools.product(config["databases"], config["workloads"]))
    rows = [
        [
            f"{backend}/{version}/{filesystem}",
            *[config["execution_status"] for _ in columns],
        ]
        for backend, versions in config["storage_versions"].items()
        for version, filesystem in itertools.product(versions, config["filesystems"])
    ]
    print("# Planned VM performance matrix\n")
    print(
        f"{len(rows)} configurations × {len(columns)} cases = {len(rows) * len(columns)} runs.\n"
    )
    print(
        "\n".join(
            table(
                ["Configuration", *[f"{db}/{workload}" for db, workload in columns]],
                rows,
            )
        )
    )
    env = config["environment"]
    print(
        f"Whole stack in guest; RAM starts at {env['initial_ram_mib']} MiB, final common size unmeasured; {env['vcpus']} vCPU; no swap."
    )
    print(
        f"{env['logical_capacity_mib']} MiB logical capacity; backing size unmeasured; {config['value_bytes']}-byte values; {config['operations_per_case']} requests/case."
    )
    print(
        "128 MiB devices retain the same RAM budget; their workload/FS fit is not validated. PostgreSQL is not included in these 48 performance cases.\n"
    )
    print("Remaining before execution:\n")
    for item in config["remaining"]:
        print(f"- {item}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("matrix", help="show all planned configurations and readiness")
    report = commands.add_parser(
        "report", help="write Markdown, CSV and JSON from evidence directories"
    )
    report.add_argument("inputs", type=Path, nargs="+")
    report.add_argument(
        "--output",
        type=Path,
        required=True,
        help="new output directory; never overwrite an existing report",
    )
    report.add_argument(
        "--case",
        action="append",
        default=[],
        help="case name/glob; repeat to select several",
    )
    args = parser.parse_args(argv)
    try:
        if args.command == "matrix":
            matrix()
            return 0
        if len(args.inputs) > 100:
            raise ValueError("at most 100 evidence runs per report")
        runs = [read_run(path) for path in args.inputs]
        if len({run["source"] for run in runs}) != len(runs):
            raise ValueError("duplicate evidence input")
        if len({run["id"] for run in runs}) != len(runs):
            raise ValueError("run directory names must be unique")
        for run in runs:
            if args.case:
                run["rows"] = [
                    row
                    for row in run["rows"]
                    if any(
                        fnmatch.fnmatchcase(row["case"], pattern)
                        for pattern in args.case
                    )
                ]
        path = write_report(runs, args.output)
        print(
            f"Report: {path}\nSamples: {path.parent / 'cases.csv'}\nStructured data: {path.parent / 'report.json'}"
        )
        return (
            0
            if all(
                run["status"] in ("pass", "validated-full-matrix", "calibration")
                for run in runs
            )
            else 1
        )
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(2, f"workbench: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
