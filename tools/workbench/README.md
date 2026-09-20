# Read the matrix and compare results

Run these commands from the repository root. Reporting is read-only with
respect to source evidence and needs no VM, mount, device access or sudo.

## Which configurations and tests are planned?

```sh
uv run --quiet python tools/workbench/workbench.py matrix
```

The table expands [matrix.json](matrix.json): two VDO filesystem baselines,
plus our candidate, v1.0 and v1.1 on both filesystems. Each configuration
selects SQLite/RocksDB with A, F and E75: 48 performance runs in total.
It shows RAM, capacity and execution readiness. PostgreSQL remains a separate
correctness/sizing requirement and is not silently added to the performance matrix.

The VM execution command is still missing. The current boot probe cannot run
this matrix. Its remaining integration is tracked in
[workbench readiness](../../docs/WORKBENCH-READINESS.md). When execution is
integrated, a selected configuration and test set must produce the report
automatically and print its location, including when a case fails.

## Read results without inspecting raw logs

Compare the accepted SQLite/ext4 and SQLite/XFS matrices:

```sh
uv run --quiet python tools/workbench/workbench.py report \
  evidence/sqlite-1789917416605635188 \
  evidence/sqlite-1789917155929943435 \
  --output /tmp/sqlite-comparison
```

Select specific cases with repeated `--case` arguments. Quote globs for zsh:

```sh
uv run --quiet python tools/workbench/workbench.py report \
  evidence/sqlite-1789917155929943435 \
  --case speedtest-main --case 'kv-random-update-*' \
  --output /tmp/xfs-selected
```

Each output directory contains:

| File | Purpose |
|---|---|
| `report.md` | Run status, a table comparing case times, compact KPIs and latency tables |
| `cases.csv` | One row per case/repetition for spreadsheet comparisons |
| `report.json` | Structured rows, configurations, source paths, per-operation latency counts and failures |

The CLI prints all three paths. It refuses an existing output directory so
rerunning cannot replace a report accidentally. A failed or incomplete run
produces a report and exit code 1; invalid input or output errors use code 2.
Code 0 means the selected evidence was processed successfully, not that an
I/O-stress benchmark or VM suite passed.

## What do the KPIs mean?

The report includes initialization time, workload time, workload-plus-drain
time, completed logical operations, operations/second, backing read/write MiB,
backing data-I/O counts and separate flush counts, peak process RSS, and API-call
p99/p99.99 where collected. Occupied physical space remains unavailable in
these inputs. Missing metrics are `N/A` in Markdown, blank in CSV and `null`
in JSON. They are never reported as zero.

SQLite backing counters cover workload plus drain, including the recorded
residual segment. They exclude load, verification, host-device amplification
and space occupancy. Its times use the existing in-process measurements.
The complete 87-case suites pass through the existing strict baseline validator.
Individual pass records retain their logical-validation and ledger checks.

YCSB inputs are the original `raw-ext4-timing-*/report.json` directories,
not the manually combined portfolio summaries. They remain labelled as cached
host-ext4 calibration with 512-byte sectors. Logical throughput divides requests
by process elapsed time, including database open/close. Workload F executes
extra update calls; those are not extra logical requests. Latencies come only
from the final cumulative log summary. The report retains operation counts and
estimated observations above p99.99; sparse tails are weak evidence.

Repeated SQLite cases show medians; CSV/JSON preserve every sample. The report
does not average interval percentiles, merge different evidence classes,
rank implementations or infer regression percentages across unlike settings.
Use `tools/sqlite/baseline.py compare` for its stricter matched-baseline workflow.

## Generated examples

- [SQLite/ext4 and XFS](../../evidence/reports/sqlite-filesystems-2026-09-20/report.md): 174 verified case repetitions.
- [YCSB A/F/E75 timing calibration](../../evidence/reports/ycsb-calibration-2026-09-20/report.md): six cases, with original scopes and missing metrics explicit.

Reproduce the second example with all three timing directories and
`--case a --case f --case e75` to exclude the historical E95 cases.
This command exits with code 1: the first source batch has completed SQLite
cases but no final batch status or cleanup confirmation. The report preserves
that incomplete status; filtering its cases does not turn the batch into a pass.
The SQLite raw evidence can be restored from
`evidence/sqlite-xfs-acceptance-2026-09-20.tar.gz` if those ignored directories
are absent from a fresh checkout.

Run the reporter regression tests:

```sh
uv run --quiet python -m unittest discover -s tools/workbench -p 'test_*.py'
```
