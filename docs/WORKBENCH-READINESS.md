# Workbench integration findings

Date: 2026-09-20
Status: native go-ycsb driver smoke passed; live workloads not integrated
Related: [workbench proposal](../TMD-WORKBENCH.md), [ADR-06](../ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md)

## Matrix and results interface

The [reporting CLI](../tools/workbench/README.md) displays all 48 planned
performance cases and produces Markdown, CSV and JSON from selected SQLite
and YCSB evidence directories. Generated examples cover 174 SQLite repetitions
and the six A/F/E75 timing cases. It preserves failure states, source paths,
measurement scope and unavailable metrics. The VM configuration/test launcher
is still unimplemented; the planned matrix explicitly reports `not_ready`.

## Fixed-memory benchmark machines

The [VM environment decision](WORKBENCH-VM.md) fixes both backends inside the
guest, separates OS and test disks, and defines the planned CPU isolation.

The user selected the entire stack inside KVM guests with swap disabled.
Start at 320 MiB and increase until all selected stacks, including PostgreSQL,
fit. Use the same final RAM size for 128 MiB and 4 GiB logical devices and for
both storage backends. Logical capacity and larger backing capacity remain
separate from RAM sizing. The final common RAM size is not measured yet.

The [320 MiB boot probe](../evidence/vm-320m-1789931760947726172/console.log)
passed with ublk/ext4/XFS and no swap. It reported 269,788 KiB `MemTotal`
and 206,448 KiB available before database startup. The runner now accepts
320..4096 MiB, defaults to 320 MiB and rejects invalid sizes before launching
QEMU. This does not prove that the complete database/storage stack fits yet.

The earlier 256 MiB prerequisite probe used a minimal guest with
one vCPU booted successfully using `tools/vm/setup.sh` and `tools/vm/boot-probe.sh`.
It loaded ublk and XFS, confirmed ext4 support, reported 206,300 KiB usable RAM
and 147,896 KiB available before database startup, and powered off cleanly.
The [console evidence](../evidence/vm-256m-1789931259345264680/console.log) and
adjacent command/build manifest preserve the result. The database image and
virtual test disks are not integrated yet.

The 32 MiB calibration dataset still needs resizing or further cache constraints
before it qualifies as a read-I/O workload. The exact dataset remains open;
the measured operation budget stays at 131,072 requests per case.
Both storage backends run inside the guest. VDO's documented base/cache
footprint is part of sizing, alongside the database and kernel. See the
[current plan](V1.3-BENCHMARK-PLAN.md).

## go-ycsb checkout

The local checkout is `/home/miguel/play/go-ycsb`, commit
`427715abeef2186b123cc93ced4acb385ff59ce1`. Tracked files were clean; the user's
untracked `wkload.file` was left unchanged. No source files or existing binaries
in that checkout were changed.

The source includes SQLite and RocksDB drivers behind the `libsqlite3` and
`rocksdb` build tags. The existing `bin/go-ycsb` has neither tag. Executing a
one-record load with explicit disposable paths reports `sqlite is not registered`
and `rocksdb is not registered`. Checksum-pinned packages now supply native headers
under `tools/build/workbench-native/root`. The original combined build failed
because its 2019 RocksDB binding calls the removed
`rocksdb_block_based_options_set_block_cache_compressed` function.

`make setup-go-ycsb` builds a private source export with the maintained binding
`github.com/linxGnu/grocksdb` v1.9.9, whose upstream build targets RocksDB 9.11.1.
The adaptation changes the import and four option widths. Both drivers passed
all six core workloads with 32 initial records, 4 KiB values and 128 logical
operations per fresh database. SQLite also passed independent integrity,
row-count and value-length checks. The [smoke evidence](../evidence/workbench-go-ycsb-smoke-2026-09-20.json)
records the result. These host-filesystem probes are not ublk acceptance or
performance baselines. Workload F's `TOTAL` counts database calls; subtract its
additional update calls when checking the configured logical-operation count.

These source findings constrain integration:

| Requirement | Verified behavior at the pinned commit |
|---|---|
| Reproducible requests | `pkg/workload/core.go:143` seeds each worker with `time.Now().UnixNano()`. Add and record a seed before claiming matching logical request sequences. |
| Explicit skew | `core.go:665` passes the fixed `generator.ZipfianConstant` (0.99). The stock workload A file actually selects `uniform`, despite its Zipfian comment. Set properties explicitly. A 1.35 exponent needs an adaptation; verify generator support before choosing it. |
| SQLite durability | `db/sqlite/db.go:72-82` configures journal mode but provides no synchronous-mode property. Apply and read back the required settings on the benchmark connections. |
| RocksDB durability | `db/rocksdb/db.go:98-105` constructs default write options. `rocksdb.use_fsync` chooses a synchronization mechanism; it does not set per-write `sync=true`. |
| Success accounting | `pkg/client/client.go:140-146` prints operation errors and still increments the completed-operation counter. The lab must reject operation errors and verify successful counts. |
| Logical verification | `core.go:247-251` accepts an empty result during deterministic-value verification. Keep an independent full key/value check before and after restart. |
| Ordered scans | SQLite `db.go:237-246` omits `ORDER BY YCSB_KEY`. RocksDB `db.go:195-217` returns its full preallocated result slice, including nil rows after a short scan. `core.go:509` discards scan contents. Fix these before accepting scan results. |
| E workload definition | The supplied `workloade` sets `maxscanlength=1`; the new local profiles request 16 records and ordered, zero-padded insert keys. Uniform scan starts remain in the initial loaded range. |
| Tail latency | `pkg/measurement/histogram.go` already computes p99.99 with HDRHistogram. Retain per-operation counts and raw histograms; sparse tails need an explicit label. |

The local `tools/go-ycsb/lab.patch` now supplies a fixed workload seed, verifies
SQLite FULL/cache/mmap settings, enables RocksDB synchronous writes, sets its
block cache and disables its compression. It adds SQLite's ordering clause and
fixes RocksDB's short-scan result length and native scan-value leak. These are
local adaptations; the source findings above describe the unmodified commit.
The timing runner checks sync calls and operation/row counts, but independent
scan-content verification and the full restart lifecycle remain open.

The six-case [host-ext4 timing calibration](../evidence/raw-ext4-portfolio-timing-2026-09-20.json)
completed with synchronous-write probes, matching operation counts and database
row-count checks. Load plus workload took 148.86 seconds. Reads were cached;
this is a duration estimate, not final I/O-stress acceptance. The final SQLite
settings helper also passed a fresh eight-record load after adding a 30-second
context bound. The vendored-code lint check retains two findings: the seeded
benchmark PRNG triggers G404, and the external baseline forbids upstream-style
`fmt.Errorf` in favor of a different errors dependency. No lint rule was disabled.

The same workload definition does not imply identical block traces: database
behavior and timing differ. `--target` throttles attempted operations; it does
not guarantee deterministic idle intervals under scheduler or database stalls.

The local E95/E50 property files passed tiny native probes on both drivers:
64 loaded records and 256 requests per case. SQLite's independent check verified
integrity, the final record count, consecutive 20-digit keys and 4,096-byte values.
The [probe evidence](../evidence/workbench-ycsb-e-properties-2026-09-20.json) does
not establish scan-result correctness, fixed seeds, memory controls or durability.

## Remaining ADR-06 tooling

The performance portfolio now selects A/F/E75 only. The two E75 cases passed
the [host-ext4 timing probe](../evidence/raw-ext4-portfolio-e75-timing-2026-09-20.json),
bringing the estimated per-scenario load/run duration to about 2 minutes
45 seconds when combined with the existing A/F measurements. PostgreSQL's
driver is present and imported by go-ycsb, but its server-on-ublk lifecycle and
ordered scan validation still need integration. Adding PostgreSQL to all four
performance scenarios would expand the matrix from 24 to 36 cases.

Installed RocksDB tools are `db_bench`, `ldb` and `sst_dump`, package version
`9.11.2-1`. `make setup-rocksdb` successfully built the missing `db_stress` and the
native benchmark/checking tools from pinned sources with GCC 13. The generated
artifact hashes and build manifest live in `tools/build/rocksdb-9.11.2/`.
The v9.11.2 source audit established:

- `verification_only=true` with persisted `expected_values_dir` verifies existing
  contents without running the logical workload. `read_only=true` instead
  repopulates before reopening and is unsuitable for restart verification.
- Several fault-injection frequencies default to 1000. Every fault option must be
  explicitly disabled for ADR-06.
- `db_bench`'s `flush` waits and checks errors. `waitforcompaction` calls
  `WaitForCompact` but only prints its status; the lab must check that status.
- Both tools leave final close status unchecked. Strict acceptance still needs
  checked finalization and measured drain boundaries in the workload process.

PostgreSQL 18.6 client tools are installed. The inspected `/usr/lib/postgresql/18/bin`
does not contain `postgres`, `initdb`, `pg_ctl`, `pgbench` or `pg_checksums`.
The checksum-pinned server package has since been extracted by `tools/native/setup.sh`
under `tools/build/workbench-native/root`; its `postgres --version` reports 18.6.
No system package was installed and no system PostgreSQL service was started.
Its filesystem acceptance is still unimplemented.

## Supplied LVM volumes

The host exposes `/dev/ubuntu-vg/blk-128mib-0` and `/dev/ubuntu-vg/blk-4gib-0`.
They were not written. Their existence alone is not a backing-device safety check.
The workbench must validate identity, capacity, mounts and use before formatting.
The earlier finite-log sizing suggestion was 1 GiB backing for the 128 MiB logical
scale and 8 GiB for the 4 GiB scale. The small size matches existing acceptance;
8 GiB was a proposed budget, not a measured minimum. The longer insert-heavy
performance runs require new write-budget calibration. The workbench's
approximately 192 MiB and 5.3 GiB figures are draft estimates for ADR-08 reclamation.
The accepted XFS fixture exposes 512 MiB because the system's default formatter
does not support the proposed 128 MiB XFS scale.

ADR-06's SQLite acceptance continues to use owned regular-file fixtures. Raw-volume
support, trace capture/replay and simulation remain separate workbench deliveries.
Aging into steady state requires ADR-08 reclamation; the current engine's log is finite.

The release target is v1.3. Run the [benchmark portfolio](V1.3-BENCHMARK-PLAN.md)
and their LVM-VDO comparisons only after ADR-06 acceptance is complete. The
revised performance plan keeps A and F, adds only E75 (75% scans, 25% inserts), and consolidates each
original three-run budget into one run, rounded up to 131,072 requests, with 4 KiB values,
bounded database and OS caches, and separate physical-space measurements.
VDO starts with compression and deduplication disabled and 4 KiB minimum I/O.
