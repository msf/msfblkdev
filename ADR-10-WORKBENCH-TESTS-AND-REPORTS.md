# ADR-10 Workbench tests and reports

Date: 2026-09-20
Status: accepted interface and measurement scope; VM workload execution remains open
Related: [ADR-06 correctness](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md), [benchmark plan](docs/V1.3-BENCHMARK-PLAN.md), [VM environment](docs/WORKBENCH-VM.md), [blockcheck plan](docs/BLOCKCHECK-PLAN.md)

## Context

The lab records commands, validation results, device counters and database logs
in separate evidence files. Comparing runs requires reconstructing those files
by hand. A fast database workload can also serve its reads from RAM, so its
throughput alone does not measure our block device.

The operator needs two workflows: select a configuration and tests, then read
the results; or inspect the complete matrix and compare several runs. We keep
the raw evidence and derive a small, structured report from it.

## Decision

### What questions do these tests answer?

| Suite | Goal and current scope |
|---|---|
| SQLite correctness on our device with ext4 or XFS | Verify filesystem checks, database contents and clean daemon restart. Both 87-run matrices passed. These use owned regular-file backing on the host. |
| go-ycsb host-ext4 calibration | Estimate load/workload duration and check request counts, synchronous writes and final record counts. Reads can be cached; this does not establish storage performance. |
| KVM boot probe | Verify that a small guest boots with ublk, ext4, XFS and no swap. It runs no database and attaches no test disk. |
| Planned VM comparison | Compare the complete database/filesystem/storage stack under a common RAM and CPU budget. Execution is not implemented yet. |
| Planned raw-device space and crash suite (blockcheck) | Measure endurance, write amplification, ENOSPC and write pacing, discard and crash consistency on the device, with no filesystem. Compare v1.2.3 with an ADR-08 candidate on host backing files. It needs no VM. See the [blockcheck plan](docs/BLOCKCHECK-PLAN.md). |

The performance portfolio retains A (50% reads, 50% updates), F (50% reads,
50% read-modify-write requests) and E75 (75% scans, 25% inserts) on SQLite and
RocksDB. F is not an atomic transaction. E75 requests 16 records per scan.
Each performance case uses one run of 131,072 logical requests and 4 KiB values.
The 8,192-record calibration dataset is too cacheable to establish read-I/O
stress; the final dataset still needs measurement. Correctness retains its
three repetitions and is separate from this performance budget.

The current candidate has four storage scenarios: LVM-VDO with ext4 or XFS,
and our device with ext4 or XFS. Those give 24 performance cases. Adding our
v1.0 and v1.1 versions on both filesystems gives 48 cases, with the two VDO
baselines run once. Historical versions need separate builds and fresh images.
PostgreSQL remains an ADR-06 correctness and RAM-sizing requirement; it is not
included in those 48 performance cases.

### How do I inspect the matrix and run existing tests?

Run commands from the repository root. First inspect the planned matrix:

```sh
uv run --quiet python tools/workbench/workbench.py matrix
```

The command reads [matrix.json](tools/workbench/matrix.json), expands the
configurations and workloads, and prints their readiness. `not_ready` means
that the configuration cannot yet be launched through this CLI. There is no
`workbench run` command yet.

For a live SQLite correctness test, first complete the
[normal-user ublk setup](README.md#normal-user-ublk-setup) and build the pinned
SQLite tools with `make setup-sqlite`. Preflight creates no device or mount:

```sh
make check-sqlite-xfs
```

Run one selected case on our device with XFS:

```sh
make test-sqlite-xfs-one SQLITE_CASE=speedtest-main \
  SQLITE_PROFILE=wal-full SQLITE_REPETITION=1
```

Run the complete XFS matrix with `make test-sqlite-xfs`. Use
`make test-sqlite-ext4` for the ext4 matrix. These explicit live tests create,
format and remove their own fixtures through the existing restricted helper.
They do not use the supplied LVs. Evidence goes into a unique
`evidence/sqlite-<run-id>/` directory with a manifest, summary and per-case
results. The [SQLite/XFS instructions](docs/SQLITE-XFS.md) describe the gate.

For host-ext4 duration calibration, build `make setup-go-ycsb` and
`make setup-rocksdb` serially. With a user systemd manager, cgroup v2 memory
controls, `strace`, GNU `time` and ext4 at `/var/tmp`, run:

```sh
systemd-run --user --wait --pipe --collect \
  -p MemoryMax=256M -p MemorySwapMax=0 --working-directory="$PWD" \
  "$(command -v uv)" run --quiet python tools/go-ycsb/time-ext4.py
```

This runs A/F/E75 on both databases in a temporary directory. The script checks
the memory limits and filesystem before workloads and saves evidence under
`evidence/raw-ext4-timing-<run-id>/`. These host measurements remain calibration.
Recipes, prerequisites and build identities are described in
[tools/README.md](tools/README.md).

To check guest prerequisites on the recorded host kernel with KVM access:

```sh
bash tools/vm/setup.sh
bash tools/vm/boot-probe.sh 320
```

The probe prints its evidence directory under `evidence/vm-320m-<run-id>/`.
It contains `command.txt`, `build-manifest.txt` and `console.log`. This is not
the command for running our device plus XFS and a database inside a VM.

Blockcheck has no command yet. Its plan records the one-time backing setup
and the intended `make test-blockcheck` target.

### How do I read and compare results?

Pass one or more evidence directories to the reporter. For example, compare
the accepted SQLite/ext4 and XFS runs:

```sh
uv run --quiet python tools/workbench/workbench.py report \
  evidence/sqlite-1789917416605635188 \
  evidence/sqlite-1789917155929943435 \
  --output /tmp/sqlite-comparison
```

On a fresh checkout, restore those ignored raw directories from the committed
archive first: `tar -xzf evidence/sqlite-xfs-acceptance-2026-09-20.tar.gz`.
For a newly executed test, substitute its evidence directory. Add repeated
`--case` selectors to narrow the report, for example
`--case speedtest-main --case 'kv-random-update-*'`. Quote globs in zsh.

The reporter prints three output paths:

| Output | Use |
|---|---|
| `report.md` | Read run status, compare case times and inspect compact KPI and latency tables. |
| `cases.csv` | Compare every case/repetition in a spreadsheet, including load/drain times, device counters and RSS where measured. |
| `report.json` | Read structured samples, configuration, source paths, failures and per-operation latency counts. |

The output directory must be new. Reporting requires no device access or sudo
and leaves raw evidence unchanged. Exit code 0 means the selected evidence was
processed with acceptable source status; 1 means a failed, incomplete or invalid
run is included in the report; 2 means an input or output error. A successful
report command does not establish VM or storage-performance acceptance.

The parser recognizes SQLite manifests and original go-ycsb timing reports.
It reuses the strict SQLite baseline validator for complete matrices. It reads
YCSB latency percentiles only from the final cumulative log summary and checks
their sample counts. It keeps correctness and calibration as separate result
classes. Filtering completed cases cannot turn an incomplete batch into a pass.

See the generated [174-sample SQLite comparison](evidence/reports/sqlite-filesystems-2026-09-20/report.md)
and [six-case YCSB calibration](evidence/reports/ycsb-calibration-2026-09-20/report.md).
The latter explicitly retains the first source batch's missing completion and
cleanup status, even though its SQLite cases have measurements.

### Which KPIs does the report provide?

| KPI | Meaning and availability |
|---|---|
| Load, workload and workload-plus-drain seconds | Separate initialization from measured work. SQLite uses recorded internal phase timings. YCSB calibration uses process elapsed time, including database open/close; a separate drain time is unavailable. |
| Logical requests and requests/second | Use completed logical operations when available. F's additional update calls are not additional logical requests. SQLite speedtest does not provide a comparable operation count. |
| Backing read/write MiB, data-I/O requests and flushes | Sum SQLite's three non-overlapping workload/drain/residual segments. Exclude initialization and verification. These are daemon backing counters, not host physical-device traffic. YCSB calibration has no such device counters. |
| Peak process RSS | Available from YCSB calibration. It is not total guest memory or filesystem cache usage. |
| API-call p99 and p99.99 in microseconds | Available from YCSB's final summaries, with per-operation sample counts. F's TOTAL covers its extra API calls. Sparse p99.99 tails remain labelled as weak evidence. |
| Occupied physical MiB | Not collected by the current suites. Cumulative bytes written are not occupied space; reserved LV capacity and consumed storage must also remain distinct. Planned for blockcheck from the backing file's `st_blocks`. |
| Endurance, write amplification, ENOSPC count, longest stall | Planned for blockcheck. Endurance is client bytes before the first ENOSPC divided by backing bytes. Write amplification uses the backing LV's `/sys/block` counters, not the daemon's backing counters. |
| Checker violations | Planned for blockcheck. Any read that returns content the checker does not allow fails the case, including after a crash. |

Unavailable measurements are `N/A` in Markdown, blank in CSV and `null` in JSON.
Zero remains a measured zero. Markdown shows medians for repeated SQLite cases;
CSV and JSON preserve every sample. The report does not average interval
percentiles or infer a regression verdict across different configurations.

### How will the VM comparison work?

QEMU/KVM will contain the database, filesystem, guest page cache and either
our daemon plus ublk or LVM-VDO. Use a separate OS disk and dedicated test
backing disk, with direct host I/O and working flushes. Require 4 KiB device
geometry and verify filesystem formatting. VDO compression and deduplication
start disabled.

Start RAM calibration at 320 MiB and increase in 64 MiB steps until every
selected stack fits. Then use one common RAM size for all comparisons and both
logical device sizes, without swap or ballooning. Dataset calibration must
also prove storage reads. Guest RAM does not include QEMU's host overhead.
CPU reservation, direct-I/O verification and physical-space collection remain
integration work; the boot probe does not establish them.

The eventual launcher must accept a configuration and test selection, preserve
evidence on failure, generate these reports automatically and print their paths.
[The VM decision](docs/WORKBENCH-VM.md) defines the execution boundary;
[readiness](docs/WORKBENCH-READINESS.md) records the missing implementation.

## Consequences

Operators can inspect the intended matrix and read current evidence without
reconstructing logs. Reports remain traceable to raw measurements, and missing
data cannot silently become zero or success.

This work does not close ADR-06: RocksDB and PostgreSQL acceptance remain open.
It does not establish crash durability, power-loss behavior, steady-state
reclamation, historical-version compatibility or a completed VM comparison.
The finite append log still requires a measured backing-write budget.

The reporter's regression checks run without a VM or live devices:

```sh
uv run --quiet python -m unittest discover -s tools/workbench -p 'test_*.py'
```
