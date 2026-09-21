# my-block-storage

A Linux-only log-structured block device experiment. Rust is authoritative and includes the storage engine, a serialized ublk frontend, and a bounded lab harness.

## Current status

Goals 1 and 2 are complete. ADR-03's engine, serialized ublk frontend and live regular-file `fio` acceptance gates pass. V0.8 acceptance is complete.

ADR-06 Deliveries 1 and 2 (V0.9 ext4 and V1.0 XFS) are complete. Each filesystem passed three fresh-image lifecycles, all 12 filesystem checks, exact content verification, clean daemon restarts and cleanup. The final `make lint test` passed. This does not establish crash or power-loss durability.

SQLite passed 87 fresh runs on each of ext4 and XFS across DELETE/EXTRA, WAL/FULL and WAL/NORMAL. Each matrix passed 348 filesystem checks and 174 logical validations. [SQLite/ext4](SQLITE-EXT4.md) and [SQLite/XFS](docs/SQLITE-XFS.md) record the suites and selected baselines. RocksDB and PostgreSQL remain unimplemented.

ADR-08 is implemented as release candidate **v2.0-rc**: on-disk format 4, a segmented log with two
fixed append heads, raw space reclamation by a cleaner thread, client discard and write-zeroes,
durable backing growth and a proportional heap budget. Engine and crash matrices, the live
ublk/fio gate (including a sustained overwrite that wraps the log) and the four performance
comparisons passed; the ext4, XFS and SQLite gates were not run for `v2.0-rc` because their
preflight requires a mount helper byte-identical to the build, which the implementation clone could
not install. [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md) records the decisions, review
dispositions and evidence; [ADR-08-PERF-RESULTS.md](ADR-08-PERF-RESULTS.md) the measurements.
Format-3 images are rejected without modification; there is no migration.

ADR-07 was complete at **v1.2.3**, with 336-entry mapping pages,
sorted snapshot publication, and adjacent-page prefetch after sequential demand.
v1.2.1 introduced packed data writes; v1.2.2 added batched checkpoint writes.
The original **v1.2** tag retains the earlier one-record-per-mapping-page implementation.
The version-3 engine uses paged mappings and a bounded CLOCK cache.
Engine/crash tests, memory/access measurements, live ublk/fio, ext4, XFS, and the full
87-case SQLite/ext4 regression matrix passed. [ADR-07](ADR-07-PAGED-MAPPING-AND-CACHE.md)
records the schema, compatibility rules, isolated deliveries, and validation evidence.
[The ADR-07 performance results](ADR-07-PERF-RESULTS.md) retain comparisons through v1.2.3.
The combined packing/prefetch experiment improves sequential mapping-page scan throughput
by 45–51% versus v1.2.2 at equal cache budgets. Random throughput stays within 0.5%;
ordinary sequential payload results are mixed.
The same report retains historical results and separates throughput, tails, memory, and close cost.

[ADR-05](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) consolidates the proposed fault model, failure handling, fault testing and simulation direction. Its design remains under review; implementation has not started. ADR-01 and ADR-03 remain the accepted baseline.

The completed minimum credible device sequence is:

```text
V0.4 closure
→ V0.5 overwrite semantics
→ V0.6 crash recovery
→ V0.7 serialized ublk frontend
→ V0.8 fio validation and restart recovery
```

Rust is authoritative. The Zig implementation is a completed initial experiment and may diverge.

## Design and delivery specifications

- [ADR-01: log-structured block device](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) defines the architecture, persistent format and high-level roadmap.
- [ADR-02: basic read and write](ADR-02-GOAL-1-BASIC-READ-WRITE.md) records V0.0 through V0.4 and its closure gate.
- [ADR-03: minimum credible device](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) specifies V0.5 through V0.8.
- [RALPH.md](RALPH.md) defines the bounded worker loop for one-hour implementation sessions.
- [ADR-05: fault model, failure handling and fault testing](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) owns the proposed policy and test plan, including the former simulation TMD. It does not expand ADR-03's completed scope.
- [ADR-06: filesystem and database correctness](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md) defines the acceptance sequence. Deliveries 1 and 2 and SQLite on both filesystems are complete. RocksDB and PostgreSQL remain open.
- [ADR-07: paged mapping and bounded cache](ADR-07-PAGED-MAPPING-AND-CACHE.md) defines the version-3 format, mapping cache, checkpoint publication, and bounded recovery; format 4 keeps the cache and recovery contract.
- [ADR-08: segmented log and space reclamation](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md) defines the implemented version-4 format: segments, two fixed heads, the mapping index tree, raw relocation, the two-publication freeing barrier, discard, backing growth and the heap budget.
- [Distributed reliable block storage](DISTRIBUTED_RELIABLE_BLOCK_STORAGE.md) is a non-authoritative future design note. It is not an implementation plan.

## Rust API

### Segmented log and space reclamation (format 4)

The Rust engine formats and opens ADR-08 version 4 only. Version-1, version-2, version-3 and
unknown-format images are rejected before recovery writes; there is no migration path. Use
freshly formatted disposable images.

The backing holds two checkpoint descriptors and then whole segments (8 MiB up to a 2 GiB backing,
then 16 or 32 MiB; `format --segment-bytes` overrides). New writes append on the User head and
relocated copies on the Relocation head; both share one global LSN sequence. A cleaner thread reads
sealed segments through fixed 339-block windows and returns proposals through bounded channels; the
writer validates each proposal against the current mapping and the victim's seal LSN, copies live
payloads, dirties current metadata, and frees a victim only after two checkpoint publications. Free
segments are hole-punched on regular files. Idle maintenance runs 2 ms after the last request or at
least every 100 ms; space pressure reclaims inline. `Volume::maintain`, `maintain_for` and
`wait_for_cleaner` drive maintenance from a request loop; `reclaim_stats`, `log_writes`,
`heap_budget` and `space_usage` report it.

Heap is budgeted as `max(1% of the logical volume, 16 MiB)` and divided between the write buffer,
the cleaner, the mapping cache and supporting state; a 1 GiB volume runs under a 12 MiB total.
After open, ordinary writes, reads, checkpoints and cleaning allocate nothing.

### Paged mapping and CLOCK cache

The public engine uses a resident page-pointer index and a bounded mapping cache;
it does not allocate dense per-LBA address and checksum arrays. Mapping pages hold 336 entries.

The cache preallocates slots, a `hashbrown` 0.17.1 hash map, and CLOCK reference bits.
It fills unused slots sequentially and reuses clean victims. Dirty mapping pages remain
resident until successful checkpoint publication. Cache pressure can cause a read to
publish a checkpoint before loading another page. Publication appends dirty snapshots,
persists the inactive index, and then persists its descriptor before selecting the new root.
Snapshot and index-body writes group up to 32 contiguous blocks (128 KiB) per io_uring
submission. Each batch must complete exactly; publication retains its three fsync barriers.
A failed batch latches the volume and leaves its dirty pages protected.
Dirty page IDs are sorted before snapshot encoding with preallocated scratch included
in the cache budget. When a read advances to the next mapping page, a miss can load up
to four current snapshots if their physical blocks are consecutive. Speculative pages
use unused or cold clean slots and never cause a checkpoint or overwrite resident pages.
Each page is validated separately; rejected speculative pages do not fail a valid demand
read. Short/failed I/O still latches the volume. Prefetch use and unused evictions are
reported through the existing mapping statistics.

`VolumeOpenOptions::cache_budget_bytes` overrides the requested byte budget.
The daemon exposes the same override as `serve ... --mapping-cache-bytes N`.
The automatic budget is the larger of `logical_bytes / 10_000` and the cache allocation
needed for the effective write buffer's worst-case mapping working set. The required slot
count is `min(buffer_payload_slots, volume_mapping_pages)`. Explicit smaller configurations
are rejected before recovery writes; one-slot caches remain valid with compatible buffers.
Sizing includes slot padding, hash-map buckets/control bytes, and policy bits. Hash-map
reservation is twice the slot count so the pinned implementation can rehash tombstones
in place. Cache operations allocate nothing after construction. Checkpoint publication
and bounded recovery can allocate a candidate index or replace a recovery-window cache.
The budget excludes the resident index, candidate index, I/O and payload buffers,
allocator overhead, and inline objects; it is not a process RSS limit.

Releases sort the whole buffer by LBA and pack up to 338 payloads per data record,
including payloads from different mapping pages. A flush issues one data-log fsync after
the required records complete. Cache pressure and replay-threshold checkpoints occur
before a group, with their own metadata barriers. The checkpoint interval must hold one
complete packed buffer. This replaces v1.2's one-record-per-mapping-page release rule.

`Volume::mapping_stats()` reports foreground/recovery hits and misses, evictions,
victim scans, pressure, resident/dirty slots, requested/effective cache bytes, index bytes,
metadata I/O, and checkpoint failures. A pressure retry retains its original lookup count.
Snapshot I/O counters include completed operations before a later failure; they do not
imply successful publication. Tail-scan reads are counted separately. `summary()` formats
these statistics for reporting outside timed work.
At info level, the daemon emits `event=mapping_stats` at open and before close;
the latter excludes checkpoint work performed by close. Existing lab evidence captures
these records with daemon stderr.

A flush persists the packed data records; it does not require a checkpoint or clear
dirty mapping flags. Recovery replays
those records, using bounded cache windows when the dirty working set exceeds capacity.
Admission reserves data records, footers, segment crossings and mapping snapshots before accepting
a new buffered LBA, and never lets foreground writes consume the cleaner's reserve. The log wraps:
overwrites beyond the backing's capacity are served as long as live data plus the reserve fit.

Run cache-only tests with `cd rust && cargo test --lib mapping_cache`; these need no
backing file or `io_uring`. [ADR-07](ADR-07-PAGED-MAPPING-AND-CACHE.md) records the exact
persistent schema, delivery steps, and executed validation.

The [memory and access report](evidence/adr7-mapping-profile-2026-09-18.md) contains historical 320-entry measurements and
reproduction commands for `mapping_cache_profile`, default allocation measurements,
separate process RSS snapshots, and exact hit rates for four deterministic read sequences.

### Engine API

The crate exposes:

```text
format, format_with_options
open, open_with_options
Volume::write_block
Volume::read_block
Volume::discard_blocks, Volume::write_zeroes
Volume::flush
Volume::close
Volume::maintain, Volume::maintain_for, Volume::wait_for_cleaner
Volume::grow_backing
Volume::space_usage, reclaim_stats, log_writes, heap_budget, mapping_stats
Scheduler
```

The ublk daemon adds `serve` flags for the heap (`--heap-ppm`, `--heap-bytes`, `--cleaner-bytes`),
growth capacity (`--growth-capacity-bytes`), backing discard (`--backing-discard off|punch`) and
scheduling (`--idle-wait-us`, `--max-maintenance-interval-ms`); it advertises discard and
write-zeroes, and claims an operator-grown backing file on `SIGUSR1`.

`format_with_accounting` and `open_with_accounting` add opt-in cumulative I/O budgets through a private shared ledger. The ordinary APIs remain unmetered. See the unsafe file-lifetime contract in `rust/src/io_accounting.rs` before using this API.

Tests require Linux, `io_uring`, and a temporary filesystem supporting `O_DIRECT`.

## Testing

V0.8 gates remain the regression baseline. V0.9 and V1.0 add the accepted ext4 and XFS lifecycles.

### Running every test

The suite is split by privilege, because most targets create nothing and a few
create a live device or a mount. Run them in this order: each group needs more
access than the one before it.

```sh
# 1. Unprivileged. No device, no mount, no sudo.
make lint test test-acceptance
make lint-zig test-zig          # historical Zig mirror

# 2. Live ublk device. Needs loaded ublk support, access to /dev/ublk-control, and fio.
make test-ublk-fio

# 3. Live mounts. Needs the installed sudo mount helper (see the ext4 section).
make test-ext4
make test-xfs

# 4. SQLite on a live mount. Run setup-sqlite once; it is pinned to 3.51.3.
make setup-sqlite
make test-sqlite                # one fresh ext4 WAL/FULL probe
make test-sqlite-ext4           # 87-run fail-fast matrix
make test-sqlite-xfs            # the same 87-run matrix on XFS
```

| Target | Requires | Creates |
| --- | --- | --- |
| `lint`, `test` | nothing | nothing |
| `test-acceptance` | nothing | regular-file backing in a temporary directory |
| `lint-zig`, `test-zig` | downloads pinned Zig on first use | nothing |
| `test-ublk-fio` | ublk, `fio` | ublk device, format-4 regular-file backing (ten 32-block segments, thin 4 MiB volume); eight scenarios including exhaustion beyond the log and a sustained overwrite that wraps it |
| `test-ext4`, `test-xfs` | mount helper, `mkfs`/`fsck` tools | ublk device, filesystem, mount |
| `test-sqlite`, `test-sqlite-ext4`, `test-sqlite-xfs` | the above plus `make setup-sqlite` | ublk device, selected filesystem, mount, databases |

Check prerequisites without creating anything:

```sh
make check-ext4 check-xfs check-sqlite check-sqlite-ext4 check-sqlite-xfs
```

To repeat one matrix member instead of all 87:

```sh
make test-sqlite-ext4-one SQLITE_CASE=speedtest-main \
	SQLITE_PROFILE=delete-extra SQLITE_REPETITION=1
```

Every target writes evidence under `evidence/`, and a failed run preserves its
fixture directory and prints the path. Read those files first when a target
fails: the console output is intentionally short.

### Diagnostic logging

The daemon and engine log to standard error at three levels, selected by
`BLOCK_STORAGE_LOG`:

| Level | Content |
| --- | --- |
| `off` | nothing |
| `info` | default. Lifecycle and aggregate events only, with no per-request cost |
| `debug` | adds one record per request and per log write |

Records are one line of `key=value` pairs after the level name:

```text
info event=request_totals reads=355 writes=432 flushes=191 read_sectors=10608 write_sectors=40960
debug event=request tag=3 op=write op_flags=0x801 start_sector=4096 nr_sectors=256 completion=131072
```

Use `debug` to compare the daemon's request stream against the kernel's
`/sys/block/ublkb*/stat` counters. Do not use it while measuring performance,
because one write per request perturbs timing. An unrecognized value is an
error rather than a silent fallback. Machine-read evidence, such as the
`request-count` lines the lab parses, is not logging and is always emitted.

### Development loop

Use Cargo for a focused inner loop while editing Rust. Use the top-level Makefile targets for repository gates and before each commit.

```sh
cd rust
cargo test test_name_fragment
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cd ..
make lint test
```

`make test` is the bounded developer suite. It runs the normal Rust matrix with a 15-second limit per test and a 55-second suite limit. It reports each test's duration and does not run live ublk/`fio` or the exhaustive crash matrix.

### Exhaustive engine crash acceptance

`make test-acceptance` runs the full Rust matrix in acceptance crash mode. Each process-crash scenario uses 20 fresh repetitions. Coverage includes record, mapping, log-fsync, checkpoint-body, descriptor, and all 339 stale-tail clearing boundaries. It also covers uncaught-panic recovery and corrupted-checkpoint fallback.

The lab starts only test-created children. It waits for named persistence handshakes, kills only the recorded process group, reaps descendants, and verifies recovery from regular-file backing. It does not require ublk or root access.

The default limits are 30 minutes per test and 60 minutes for the suite. The V0.8 run took about 10 minutes, dominated by the exhaustive stale-tail test. Evidence is written to `evidence/engine-crash-*.log` with the commit, kernel, backing type, commands, timings, and result.

### Live ublk and fio acceptance

`make test-ublk-fio` builds the daemon and lab with `test-failpoints`, then tests the live kernel-to-engine path. It requires Linux, `fio`, loaded ublk kernel support, and read/write access to `/dev/ublk-control`.

The target runs seven scenarios three times. Every repetition uses a newly formatted 32-record regular-file image:

1. Sequential write, flush, read, and verify.
2. Seeded random write, flush, read, and verify.
3. Eight overwrites of one logical block address.
4. Graceful `SIGTERM`, restart, and verify.
5. `SIGKILL` after a successful `fio` flush, restart, and verify.
6. `SIGKILL` at the descriptor-write failpoint, restart, and verify.
7. A 33-write exhaustion test that requires `ENOSPC`, restarts, and verifies the 32 successful writes.

Each child operation has a 30-second limit. The harness validates device identity and geometry before I/O or deletion. It proves backing-file lock contention and cleans only its recorded device and owned temporary directory. If identity becomes ambiguous, it refuses cleanup and preserves evidence. The V0.8 run took about 21 seconds. Evidence is written to `evidence/ublk-fio-*.log`.

A normal-user run can print a harmless `fio` warning that only root may invalidate a block-device cache. This is separate from the ublk FLUSH requests used for durability validation.

Run all repository gates with:

```sh
make lint test test-acceptance test-ublk-fio
```

### Normal-user ext4 development and acceptance

Build and test as the normal user:

```sh
make lint test
make build-ext4
make check-ext4
```

`make test` includes META/FAILFAST regression tests, file-workload checks, mount-table validation, helper ownership/argument checks, runtime-directory preflight, command deadlines and failure-evidence tests. It does not mount filesystems, invoke sudo or require filesystem tools. `make check-ext4` checks live prerequisites but creates no backing image, device or mount.

The live lab also runs as the normal user. It uses a separately installed root-owned helper only for mount and unmount. Complete the normal-user ublk setup in the next section. After reviewing the helper and granting operator approval, install it once:

```sh
sudo bash scripts/install-ext4-helper.sh
```

The installer copies the already-built `block-storage-mount` binary into `/usr/local/libexec` and installs a sudo rule for the invoking user. It also creates `/run/ublksrvd` as a private directory for that user. A tmpfiles rule recreates it after reboot: libublk writes its device metadata there. This setup supports one developer and refuses to take over a runtime directory owned by someone else. It does not build code, load modules, create devices or mount filesystems. Reinstall after changing the helper; preflight refuses a stale installed binary. Never grant passwordless sudo to the lab binary in the writable build directory.

The helper validates the kernel-recorded ublk owner, device geometry and the owned directory under `/tmp`. It pins device/directory descriptors and serializes helper calls with a root-owned lock. Protocol 3 permits only ext4 and XFS with `nosuid,nodev`; filesystem journaling and barrier defaults remain unchanged. It creates a caller-owned `work` directory inside the filesystem. Unmount validates the exact mount and refuses aliases, nested mounts, force and lazy unmount.

This is a privilege grant for a trusted local developer, not a sandbox for hostile filesystem images. The kernel still parses filesystem data supplied by that developer. Use a disposable VM for untrusted images.

Once the operator approves live mount operations, run:

```sh
make test-ext4
```

The gate runs three fresh-image repetitions. Each exposes a 128 MiB device backed by a 1 GiB regular file. It uses default `mkfs.ext4`, four unmounted `e2fsck -f -n` checks, synced file operations, exact content comparisons, hashes and a clean daemon restart. The accepted run took 3.989 seconds on Linux 7.0.0-31-generic with e2fsprogs 1.47.2; see `evidence/ext4-1789253771549431348.log`. No crash or power-loss guarantee is inferred.

Defaults are 60 seconds per child command and 600 seconds for the suite. `TEST_PER_TEST_SECONDS` and `TEST_SUITE_SECONDS` override the lab limits; the privileged helper has its own 60-second alarm. Failure cleanup gets one 60-second reserve plus bounded child reaping. The lab fails rather than starting another operation after its deadline.

Evidence is written to `evidence/ext4-*.log`, including versions, binary fingerprints, geometry, commands, timings and results. Failures retain the owned temporary directory, image and raw output. If mount or device identity is ambiguous, the lab preserves the daemon and reports its PID and paths for operator recovery. Do not delete those resources until their identities and mount state have been checked. On success, the lab removes its mount, device, children and temporary directory.

To revoke the helper grant, remove `/etc/sudoers.d/block-storage-ext4-<user>` and `/usr/local/libexec/block-storage-mount` as an administrator after confirming no lab run is active. Remove `/etc/tmpfiles.d/block-storage-ublk.conf` if the private runtime directory is no longer needed.

### XFS development and acceptance

[GOAL-XFS.md](GOAL-XFS.md) records the completed V1.0 scope. After operator approval and protocol-3 helper installation, run:

```sh
make check-xfs
make test-xfs
```

`check-xfs` checks tool versions, kernel support, helper identity, device access, memory and temporary space without creating a device or mount. `test-xfs` runs the same file workload and restart lifecycle as ext4, using three fresh 512 MiB devices with 4 GiB backing files. It uses default `mkfs.xfs` and four unmounted `xfs_repair -n` checks per repetition. `fsck.xfs` is not a filesystem check.

Default `mkfs.xfs` requires privilege for Linux's `BLKBSZSET` ioctl even when the block size is already correct. Protocol 3 adds a scoped `format-xfs` action alongside `mount-xfs` and `unmount-xfs`. It runs only `/usr/sbin/mkfs.xfs` with default options on the validated, unmounted XFS fixture. The helper keeps the device descriptor, lock and 60-second alarm across exec and clears the formatter environment. The daemon, lab, workload and read-only checker remain unprivileged. No general formatter sudo grant or developer capability is needed.

The committed implementation `0a5af77` passed in 6783 ms with XFS tools 6.18.0 on Linux 7.0.0-31-generic. See `evidence/xfs-1789263094765541439.log` and the release audit. The ext4 regression passed in 3858 ms. Backing files were on tmpfs; these are correctness results, not durability or performance claims.

Evidence includes actual mount options and bounded counts for every ublk operation/flag combination from queue startup through shutdown. Reports distinguish real I/O errors from drain-rejected shutdown cancellations. Missing, inconsistent or truncated reports fail acceptance, as do real I/O errors. Both live targets retain failed images and raw output and use the existing 60-second command and 600-second suite limits.

### SQLite on ext4 and XFS

After the existing filesystem prerequisites and operator approval, run:

```sh
make setup-sqlite
make check-sqlite
make test-sqlite-ext4
make test-sqlite-xfs
```

SQLite builds are explicit and pinned to 3.51.3. Normal `make test` needs neither SQLite tools nor mounts. The 87-run gate uses three fresh repetitions, a 512 MiB cumulative backing-write limit per run, and the existing 60-second command / 600-second suite limits. The accepted run took 131.696 seconds with tmpfs backing. The helper and filesystem defaults are unchanged.

See [SQLite/ext4](SQLITE-EXT4.md) for cases, single-run selectors, measurement limits, baseline comparison, and the upstream multiprocess audit. [SQLite/XFS](docs/SQLITE-XFS.md) records its 160.073-second acceptance run, audit and separate baseline. These are clean-restart correctness results and initial timing baselines, not crash, power-loss or sustainable-throughput claims.

### Normal-user ublk setup

Load the driver if `/dev/ublk-control` is absent:

```sh
sudo modprobe ublk_drv
```

The preferred setup gives the existing `plugdev` group access to the global control node. Each created device uses `UBLK_F_UNPRIVILEGED_DEV`. The upstream owner helper then assigns `/dev/ublkcN` and `/dev/ublkbN` to the kernel-recorded owner.

Build the vendored helper as the normal user. Install the helper, library, and restrictive udev rule as an administrator:

```sh
make -C ublksrv lib/libublksrv.la ublk_user_id
sudo ublksrv/libtool --mode=install install -m 0755 ublksrv/lib/libublksrv.la /usr/local/lib
sudo ublksrv/libtool --mode=install install -m 0755 ublksrv/ublk_user_id /usr/local/sbin
sudo install -m 0755 ublksrv/utils/ublk_chown.sh /usr/local/sbin/ublk_chown.sh
sudo ldconfig
sudo install -m 0644 ublksrv/utils/ublk_dev.rules /etc/udev/rules.d/90-ublk.rules
sudo sed -i \
  -e 's/KERNEL=="ublk-control", MODE="0666"/KERNEL=="ublk-control", GROUP="plugdev", MODE="0660"/' \
  -e 's/",KERNEL/", KERNEL/g' \
  -e 's/",RUN/", RUN/g' \
  /etc/udev/rules.d/90-ublk.rules
sudo udevadm verify /etc/udev/rules.d/90-ublk.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --settle --action=add --name-match=ublk-control
```

The project does not install or reload these host files automatically. Do not install the vendored rule unchanged because it grants mode `0666` on `/dev/ublk-control`. Do not run Make or Cargo as root because they create root-owned build artifacts.

Override test limits with `TEST_PER_TEST_SECONDS` and `TEST_SUITE_SECONDS`.

## Historical Zig experiment

The Zig source remains in `src/root.zig`. Its explicit targets remain available:

```sh
make build-zig
make lint-zig
make test-zig
```

Future milestones do not require Zig changes or Zig/Rust image compatibility.

## Safety

Automated engine and ublk lab tests use disposable regular files. Any future LVM test must identify and validate an explicitly disposable target before writing. Never use the laptop's system NVMe, a mounted filesystem or an arbitrary block device.

## Current limits

The engine has one serialized writer, 4 KiB payloads, a finite log, and no compaction or wraparound. Version 3 packs up to 338 buffered payloads per data record after sorting the complete buffer by LBA. V0.6 recovers complete flushed records after process loss. V0.8 has passed live ublk and `fio` acceptance. V0.9 and V1.0 add ext4 and XFS correctness across clean restarts. SQLite also passes its ext4 and XFS clean-restart matrices. The remaining ADR-06 acceptance targets v1.3; RocksDB and PostgreSQL are still open. Medium-write faults, compaction and wraparound remain outside ADR-03.
