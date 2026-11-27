# Workbench reporting CLI

`workbench.py` prints the planned benchmark matrix from [matrix.json](matrix.json) and builds
Markdown, CSV and JSON reports from existing SQLite and YCSB evidence. It reads evidence only and
needs no VM, mount, device access or sudo.

```sh
uv run --quiet python tools/workbench/workbench.py matrix
uv run --quiet python tools/workbench/workbench.py report <evidence-dir>... --output <new-dir>
```

[BENCHMARKING.md](../../docs/BENCHMARKING.md#read-the-matrix-and-reports) describes the commands,
outputs, exit codes and KPIs. [ADR-10](../../ADR-10-WORKBENCH-TESTS-AND-REPORTS.md) records the
interface decision.
