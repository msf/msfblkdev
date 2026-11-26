# my-block-storage

A Linux-only log-structured block device experiment. Rust is authoritative and includes the storage engine, a serialized ublk frontend, and a bounded lab harness.

## Current status

The latest stable release is [**v2.0**](https://github.com/msf/msfblkdev/releases/tag/v2.0) (`031d778`). It uses on-disk format 4 and reclaims overwritten blocks. It cannot open format-3 images from v1.2.3, and there is no in-place migration. `main` also has two engine fixes from ADR-05 that are not in a release yet.

The table shows how much of each ADR is in the code. The percentages are estimates. They compare each ADR's done deliverables with its open ones. Dropped items and items that a later ADR replaced are not counted. **Open** means planned work. **Dropped** means we accept that the item will not be done. **Deferred** means planned for later and not counted. Some ADR files still describe dropped items as planned; this table takes precedence.

| ADR | Subject | Implemented | Not implemented |
| --- | --- | --- | --- |
| [01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) | Architecture and persistent-format rules | 100% | Dropped: full-log salvage tool. Formats 3 and 4 replaced format 1. Later ADRs own the roadmap goals 3–5. |
| [02](ADR-02-GOAL-1-BASIC-READ-WRITE.md) | Basic read and write (V0.0–V0.4) | 100% | — |
| [03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) | Minimum credible device (V0.5–V0.8) | 100% | Dropped: optional LVM operator run. Later work removed the queue-depth-1 and 4 KiB request limits. |
| [04](ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md) | Growable thin-provisioned format | ~75%, through ADR-08 | Format 2 never shipped. ADR-08 delivered thin provisioning, sparse mappings and backing growth. Open: logical growth (online resize). Dropped: checkpoint arenas, replacement-image compaction, salvage tool. |
| [05](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) | Fault model and fault testing | ~60% | Implemented after ADR-08. Done: a fault-injection seam under the backing I/O, a medium model, an offline image checker, seeded fault replay, live ublk faults at checkpoint B, two engine fixes (F01, F02) and an opt-in flush retry (FSYNC-01). The daemon still fails closed. Open: retry across timeouts and cancellation, checkpoint and cleaner fault schedules, other native hosts, operator-run medium and application tests. Dropped: checksum chaining. |
| [06](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md) | Filesystem and database correctness | ~75% | Done: ext4, XFS and SQLite on both, requalified on format 4. Open: RocksDB (the tools build; no lab run). Dropped: PostgreSQL. |
| [07](ADR-07-PAGED-MAPPING-AND-CACHE.md) | Paged mapping and bounded cache | 100% | Format 4 replaced the format-3 schema. It keeps the cache, prefetch and bounded recovery. |
| [08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md) | Segmented log and space reclamation | ~90%, released in v2.0 | Open: aged SQLite A/F; crash tests for the existing discard and windowed-recovery failpoints. Dropped: Delivery 0 simulator, one-head and multi-victim experiments, bounded selection scan, `BLKDISCARD` for block-device backing. |
| [09](ADR-09-RELOCATION-COMPRESSION.md) | Compression at relocation | **Not implemented** | Deferred draft. Format 4 already rejects the reserved record kind 6 and mapping bit 31. |
| [10](ADR-10-WORKBENCH-TESTS-AND-REPORTS.md) | Workbench tests and reports | ~70% | Done: reporting CLI, test matrix, pinned tool recipes, KVM boot probe, blockcheck checker library. Open: blockcheck device runs. Deferred: the comparison against LVM thin provisioning and LVM-VDO in memory-bounded VMs; the KVM boot probe is kept for it. Dropped: the v1.x historical comparison, the TMD-WORKBENCH layers. |
| [11](ADR-11-BLKCTL-INSPECTION-TOOL.md) | `blkctl` volume inspection and offline check | ~90% | Done: `open_live` with its ownership claim, `open_readonly`, `blkctl info` and `blkctl check`, which the filesystem lab runs after each clean stop. Open: the 500 GiB `info` timing (after TODO-2) and runs on a block-device backing (after BUG-1). |

Not implemented:

- ADR-09 is a deferred draft.

ADR-08 shipped through [PR #3](https://github.com/msf/msfblkdev/pull/3): 22 commits, with acceptance logs in `evidence/adr08-implementation/` and a [technical review](docs/REVIEW-ADR-08.md).

The evidence for these results:

- The [v2.0 qualification](docs/V2.0-QUALIFICATION.md) covers engine and process-crash tests, live ublk/fio, ext4, XFS, and 87 SQLite clean-restart cases on each filesystem. It does not cover aged SQLite A/F or power loss.
- [SQLite/ext4](docs/SQLITE-EXT4.md) and [SQLite/XFS](docs/SQLITE-XFS.md) record the suites and baselines. Each matrix runs DELETE/EXTRA, WAL/FULL and WAL/NORMAL.
- The [ADR-07 performance results](ADR-07-PERF-RESULTS.md) compare v1.2 to v1.2.3. The [v2.0-rc performance report](ADR-08-PERF-RESULTS.md) records measurements before the release.
- The [ADR-05 campaign README](docs/adr05/README.md) lists the fault results, the two engine fixes and the open gates. The [fault results explorer](docs/adr05-report.html) links each case to its trace. The campaign was qualified on native arm64.
- The [blockcheck plan](docs/BLOCKCHECK-PLAN.md) specifies the device-level checks that are still open.

The filesystem and SQLite results are clean-restart correctness results. They do not establish crash or power-loss durability.

## Next work

The open items, in priority order.

1. Add crash tests that use the existing `discard-write-complete` and `recovery-snapshots-fsync-complete` failpoints (ADR-08). The released format has these crash points, but no test kills the process at them. The failpoint harness already exists, so the cost is small.
2. Finish the ADR-05 retry work in the [campaign plan](docs/ADR05-CAMPAIGN.md): retry ownership across timeouts and cancellation, then checkpoint and cleaner faults. Extend the live checkpoint-B tests to the other publication boundaries.
3. Run the [blockcheck device workloads](docs/BLOCKCHECK-PLAN.md), including aged SQLite A/F (ADR-08, ADR-10). The release has not been tested under aged database workloads.
4. Support block-device backing (BUG-1). The engine and the daemon assume a regular file.
5. Complete RocksDB correctness on ext4 and XFS (ADR-06).
6. Later: logical growth (ADR-04), then a distributed replicated log.

## Design and delivery specifications

- [ADR-01: log-structured block device](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) defines the architecture, persistent-format rules and high-level roadmap.
- [ADR-02: basic read and write](ADR-02-GOAL-1-BASIC-READ-WRITE.md) records V0.0 through V0.4 and its closure gate.
- [ADR-03: minimum credible device](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) specifies V0.5 through V0.8.
- [ADR-04: growable thin-provisioned format](ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md) proposes logical and physical growth. ADR-08 delivered physical growth; logical growth is open.
- [ADR-05: fault model, failure handling and fault testing](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) records the format-4 retry decision and the fault-testing plan. Start with the [campaign README](docs/adr05/README.md).
- [ADR-06: filesystem and database correctness](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md) defines the acceptance sequence for filesystems and databases.
- [ADR-07: paged mapping and bounded cache](ADR-07-PAGED-MAPPING-AND-CACHE.md) defines the mapping cache, checkpoint publication and bounded recovery. Format 4 keeps these contracts.
- [ADR-08: segmented log and space reclamation](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md) defines format 4: segments, two fixed heads, the mapping index tree, raw relocation, the two-publication freeing barrier, discard, backing growth and the heap budget.
- [ADR-09: compression at relocation](ADR-09-RELOCATION-COMPRESSION.md) is a deferred extension of ADR-08.
- [ADR-10: workbench tests and reports](ADR-10-WORKBENCH-TESTS-AND-REPORTS.md) defines test goals, commands and KPIs. [TMD-WORKBENCH](TMD-WORKBENCH.md), the [VM environment](docs/WORKBENCH-VM.md) and the [v1.3 benchmark plan](docs/V1.3-BENCHMARK-PLAN.md) support it.
- [ADR-11: blkctl volume inspection and offline check](ADR-11-BLKCTL-INSPECTION-TOOL.md) defines the read-only open, the ownership claim of `open_live` and the `blkctl` tool.
- [Distributed reliable block storage](DISTRIBUTED_RELIABLE_BLOCK_STORAGE.md) is a non-authoritative future design note. It is not an implementation plan.

## Rust API

### Segmented log and space reclamation (format 4)

The Rust engine formats and opens ADR-08 version 4 only. It rejects version-1, version-2, version-3
and unknown-format images before recovery writes anything. There is no migration path. Use freshly
formatted disposable images.

The formatter requires the advertised logical size and the raw backing size. For example:

```sh
block-storage-ublk format disk.img --size-mib 128 --prealloc-mib 185
```

`--min-logical-fill-factor` sets the minimum percent of the **logical** volume that must fit as live payload while leaving room for cleaning. It defaults to `100`: for the 128 MiB volume above, all 128 MiB must fit. The formatter tries 32, 16, then 8 MiB segments and picks the largest size that meets the requested fill. If none fits, it rejects the request before creating the image. The reported maximum logical fill is a sizing estimate, not a measured endurance guarantee. The raw size sets the sparse file's length; it does not reserve that much free space on the host filesystem.

The backing holds two checkpoint descriptors, then whole segments. New writes append on the user head. Relocated copies append on the relocation head. Both heads share one global LSN sequence.

Space reclamation works in this way:

- A cleaner thread reads sealed segments through fixed 339-block windows.
- The cleaner returns proposals through bounded channels.
- The writer validates each proposal against the current mapping and the victim's seal LSN.
- The writer copies the live payloads and dirties the current metadata.
- The writer frees a victim only after two checkpoint publications.
- Free segments are hole-punched on regular files.

Idle maintenance runs 2 ms after the last request, or at least every 100 ms. Space pressure reclaims
inline, inside the write. `Volume::maintain`, `maintain_for` and `wait_for_cleaner` drive
maintenance from a request loop. `reclaim_stats`, `log_writes`, `heap_budget` and `space_usage`
report it.

The heap budget is `max(1% of the logical volume, 16 MiB)`. It is divided between the write buffer,
the cleaner, the mapping cache and the supporting state. A 1 GiB volume runs under a 12 MiB total.
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
Before it accepts a new buffered LBA, admission reserves space for the data records, the footers,
the segment crossings and the mapping snapshots. Foreground writes never consume the cleaner's
reserve. The log wraps: overwrites beyond the backing's capacity are served as long as the live data
plus the reserve fit.

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
open_live, open_live_with_options
open_readonly, VolumeView
check
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

The ublk daemon adds these `serve` flags:

- heap: `--heap-ppm`, `--heap-bytes`, `--cleaner-bytes`;
- growth capacity: `--growth-capacity-bytes`;
- backing discard: `--backing-discard off|punch`;
- scheduling: `--idle-wait-us`, `--max-maintenance-interval-ms`.

The daemon advertises discard and write-zeroes. On `SIGUSR1` it claims an operator-grown backing
file.

`format_with_accounting` and `open_live_with_accounting` add opt-in cumulative I/O budgets through a private shared ledger. The ordinary APIs remain unmetered. See the unsafe file-lifetime contract in `rust/src/io_accounting.rs` before using this API.

Tests require Linux, `io_uring`, and a temporary filesystem supporting `O_DIRECT`.

### Ownership of the backing

`open_live` owns the backing until the `Volume` is dropped. It claims a regular file with
`flock(LOCK_EX)` and a block device with `O_EXCL`, before recovery writes anything. A second
`open_live` or daemon on the same regular file fails with `backing file is already locked`.
`open_readonly` reads one checkpoint without writing or claiming, so it works while a daemon
serves the backing. `check` takes a shared claim, so it refuses a backing that a live volume owns.

## Inspect a backing with blkctl

`blkctl` reports the state of a format-4 backing and checks its integrity
([ADR-11](ADR-11-BLKCTL-INSPECTION-TOOL.md)). Build it with `cargo build --release --bin blkctl`
in `rust/`.

```sh
blkctl info  [--segments] [--blocks] [-v] [--color=auto|always|never] <backing>
blkctl check [-v] [--color=auto|always|never] <backing>
```

`info` reads the newest valid checkpoint and prints the logical, mapped, provisioned and
host-allocated sizes, the used, live, metadata, dead and free space of the log, both
checkpoint roots, the records that wait for replay, and a histogram of the live fraction of
sealed segments. `--segments` adds one row per segment and `--blocks` prints 4 KiB block counts
instead of sizes. `info` is safe on a backing that a daemon serves; its counts are as of the
checkpoint, so they lag the writer by up to one checkpoint interval.

`check` reads every non-free segment: descriptors, index, mapping and segment table pages,
record chains, payload checksums, mapping references and the replay tail. Run it on an offline
backing; it exits 8 while a daemon or another `open_live` owns the backing.

| Exit | `info` | `check` |
| --- | --- | --- |
| 0 | Every descriptor and page it read is valid. | No errors, no tail. |
| 1 | Not used. | No errors. The next `open_live` replays a tail. |
| 4 | A descriptor or page failed validation. The view still prints if one root is valid. | Errors found. |
| 8 | Could not run: I/O error, not a format-4 backing, or the backing changed faster than the walk. | Could not run: I/O error, not a format-4 backing, or a live volume owns the backing. |

The filesystem lab runs `blkctl info` and `blkctl check` after each clean daemon stop, and
`make test-compaction-small` runs them on its closed backing.

## Testing

On native Debian 13 arm64, run `make build` to build the Rust engine, ublk daemon and test harness. Cargo uses the guest's native target (`aarch64-unknown-linux-gnu`); no cross-compiler is needed. For the live tests, use a VM with its own kernel and follow the [normal-user ublk setup](#normal-user-ublk-setup).

Run `make test-compaction-small` for the opt-in compaction regression: 128 MiB logical on 185 MiB of raw backing, at the formatter's default 100% logical-fill target. The larger fio workloads in the [release report](docs/V2.0-QUALIFICATION.md) are historical qualification runs, not part of this target.

### Set up a machine

Choose one setup. Both end with a machine that can run every step of `make test-all`.

| Setup | Host | Commands |
| --- | --- | --- |
| Native | Debian 13 or Ubuntu 24.04+ on a machine or a VM with its own kernel | `bash scripts/setup.sh`, then `make test-all` |
| VM | macOS, or Linux with KVM | `make vm-setup`, then `make vm-test` |

A container cannot load `ublk_drv`, so it can run only the unprivileged steps. No spare disk is necessary: every test uses sparse regular files under `/tmp`. Never point a test at a real block device.

#### Native setup

Run the script as the normal user from a fresh checkout. It uses `bash` because a fresh machine might not have `make`; after the first run, `make setup` does the same thing.

```sh
bash scripts/setup.sh
```

The script lists its system changes and asks before it continues. Set `YES=1` to skip the question. It checks the current state at each step, so you can run it again safely. It does these steps:

1. Installs the packages, including `rustup` if no Rust installation is present.
2. Installs the stable Rust toolchain with `rustfmt` and `clippy`.
3. Clones ublksrv at the tested revision into `./ublksrv`, builds it, and installs its owner helper and library under `/usr/local`.
4. Installs a udev rule that gives group `plugdev` access to `/dev/ublk-control`, and adds you to `plugdev`.
5. Loads `ublk_drv` and `xfs`, and loads them again at each boot through `/etc/modules-load.d/block-storage.conf`.
6. Builds and installs the mount helper with its passwordless sudo rule (see [Normal-user ext4 development](#normal-user-ext4-development-and-acceptance)).
7. Builds the pinned SQLite tools.
8. Runs the preflight checks, which create no device or mount.

The `plugdev` membership normally applies at your next login. `make test-all` does not wait for it: when the current session lacks the group, it runs itself again through `sg plugdev`.

The machine needs at least 6 GiB free in `/tmp` and 1 GiB of available memory: the XFS preflight requires 4 GiB of temporary space and 512 MiB of memory. Setup needs network access.

Run the script again after a Rust or dependency upgrade. The ext4 and XFS steps refuse an installed mount helper that differs from the current build, and the script reinstalls it.

#### VM setup

The VM setup runs the native setup inside a disposable Lima VM. It needs [Lima](https://lima-vm.io/docs/installation/) 2.0 or later: `brew install lima` on macOS; on Linux, Lima with `qemu-system` and read/write access to `/dev/kvm`.

```sh
make vm-setup                            # create the VM and run scripts/setup.sh in it
make vm-test                             # run make test-all in the VM and copy the results back
less evidence/vm-latest/test-all-latest/summary.md
```

The VM is named `msfblkdev`; set `VM_NAME` to use another name. [tools/vm/lima-debian13.yaml](tools/vm/lima-debian13.yaml) defines it: a dated Debian 13 generic image with a verified digest, 4 vCPUs, 6 GiB of memory, a 30 GiB disk and no host mounts. Lima uses VZ on macOS and QEMU on Linux. Each `vm-setup` and `vm-test` copies the current commit and your uncommitted, non-ignored files into the guest. Build output stays separate on each side. `make vm-test` copies every evidence file of the run to `evidence/vm-test-<name>-<time>/`. Use `make vm-shell` to open a shell in the VM and `make vm-delete` to remove it.

The VM setup was tested on a Linux x86_64 host with Lima 2.3.0 and QEMU/KVM. The ADR-05 campaign used the same Debian image with Lima and VZ on macOS arm64, but `make vm-setup` has not run on macOS yet.

[Normal-user ublk setup](#normal-user-ublk-setup) and [Normal-user ext4 development](#normal-user-ext4-development-and-acceptance) describe the system changes in detail, for review or for a manual setup.

### Running every test

`make test-all` runs every test step in order of required access and writes one summary page. It continues after a failed step. It skips each step that the host cannot run and records the command that fixes the prerequisite.

```sh
make test-all
```

The summary is `evidence/test-all-<time>/summary.md`. The link `evidence/test-all-latest` always points to the newest run directory, so `less evidence/test-all-latest/summary.md` shows the latest results. The summary lists each step with its result, test count, duration, rerun command and evidence paths. Each step's full output is in the same directory. A failed step also gets its last 25 output lines on the summary page. The exit status is 0 if every step passed, 1 if a step failed, and 2 if no step failed but a step was skipped.

| Step | Make target | Requires | Creates |
| --- | --- | --- | --- |
| lint | `lint` | Rust with `rustfmt` and `clippy` | nothing |
| unit | `test` | Rust | nothing (381 tests) |
| blockcheck | `test-blockcheck-checker` | Rust | nothing; the checker uses an in-memory target |
| python-tools | `test-python` | `python3`, `libxxhash0` for the ADR-05 image checker | nothing |
| compaction-small | `test-compaction-small` | Rust | 185 MiB regular-file backing in a temporary directory |
| fault-dst | `test-dst` | Rust, `python3` | regular-file backing; runs the ADR-05 medium-model controls and checkpoint-descriptor survival |
| fsync01 | `test-fsync01` | Rust | regular-file backing; lost-writeback controls and the opt-in flush retry |
| fault-seed | `test-fault-seed` | Rust | regular-file backing; runs seed 42, replays its saved plan, and requires an identical trace |
| acceptance | `test-acceptance` | Rust | regular-file backing in a temporary directory (381 tests in crash mode) |
| ublk-fio | `test-ublk-fio` | ublk access, `fio` | ublk device, format-4 regular-file backing (ten 32-block segments, thin 4 MiB volume) |
| ext4, xfs | `test-ext4`, `test-xfs` | the above, mount helper, `mkfs`/`fsck` tools | ublk device, filesystem, mount |
| sqlite-standalone | none: `bash tools/sqlite/test-standalone.sh` | `make setup-sqlite` | SQLite files under `/tmp` |
| sqlite-probe, sqlite-ext4, sqlite-xfs | `test-sqlite`, `test-sqlite-ext4`, `test-sqlite-xfs` | the filesystem prerequisites, `make setup-sqlite` | ublk device, selected filesystem, mount, databases |

On one laptop with `/tmp` on tmpfs and warm build caches, the unprivileged steps took about 2.5 minutes in total. Earlier accepted runs of the live steps took about 6 minutes in total; the two 87-run SQLite matrices take most of that time. A first build adds several minutes.

`make test-all` does not run the ADR-05 tiers that need a prepared guest: `make test-fault`, the live checkpoint-B runs and the operator-only medium tier. The [fault-lab README](tools/adr05-fault-lab/README.md) gives their procedures. Run another seed with `make test-fault-seed SEED=<n> OUT=<new-dir>`.

`make test-all` does not run the KVM boot probe (`bash tools/vm/boot-probe.sh`). The probe does not test this repository's code. It prepares the deferred comparison against LVM thin provisioning in memory-bounded VMs (ADR-10), and it returns with that work.

The automated runs do not include the aged-fio qualification in [docs/V2.0-QUALIFICATION.md](docs/V2.0-QUALIFICATION.md). That workload writes about 86 GiB and runs only by explicit operator request.

#### Run one group by hand

Check the live prerequisites without creating anything:

```sh
make check-ext4 check-xfs check-sqlite check-sqlite-ext4 check-sqlite-xfs
```

To repeat one matrix member instead of all 87:

```sh
make test-sqlite-ext4-one SQLITE_CASE=speedtest-main \
	SQLITE_PROFILE=delete-extra SQLITE_REPETITION=1
```

Every lab target writes evidence under `evidence/`, and a failed run preserves its
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

The default limits are 30 minutes per test and 60 minutes for the suite. The format-4 run on 2026-11-25 took 80 seconds on a laptop with `/tmp` on tmpfs. Evidence is written to `evidence/engine-crash-*.log` with the commit, kernel, backing type, commands, timings, and result.

### Live ublk and fio acceptance

`make test-ublk-fio` builds the daemon and lab with `test-failpoints`, then tests the live kernel-to-engine path. It requires Linux, `fio`, loaded ublk kernel support, and read/write access to `/dev/ublk-control`.

The target runs eight scenarios three times. Every repetition uses a newly formatted thin 4 MiB volume on ten 32-block segments of regular-file backing:

1. Sequential write, flush, read, and verify.
2. Seeded random write, flush, read, and verify.
3. Eight overwrites of one logical block address.
4. Graceful `SIGTERM`, restart, and verify.
5. `SIGKILL` after a successful `fio` flush, restart, and verify.
6. `SIGKILL` at the descriptor-write failpoint, restart, and verify.
7. Writes to distinct blocks beyond the log's capacity until `ENOSPC`. At least 77 blocks must succeed, and the written prefix must survive a restart.
8. Twelve overwrites of one region, so that the log wraps several times. The last pass must verify after a graceful restart and after `SIGKILL`.

Each child operation has a 30-second limit. The harness validates device identity and geometry before I/O or deletion. It proves backing-file lock contention and cleans only its recorded device and owned temporary directory. If identity becomes ambiguous, it refuses cleanup and preserves evidence. The V0.8 run took about 21 seconds. Evidence is written to `evidence/ublk-fio-*.log`.

A normal-user run can print a harmless `fio` warning that only root may invalidate a block-device cache. This is separate from the ublk FLUSH requests used for durability validation.

To run every repository gate, use [`make test-all`](#running-every-test).

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

After operator approval and protocol-3 helper installation, run:

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

See [SQLite/ext4](docs/SQLITE-EXT4.md) for cases, single-run selectors, measurement limits, baseline comparison, and the upstream multiprocess audit. [SQLite/XFS](docs/SQLITE-XFS.md) records its 160.073-second acceptance run, audit and separate baseline. These are clean-restart correctness results and initial timing baselines, not crash, power-loss or sustainable-throughput claims.

### Normal-user ublk setup

Load the driver if `/dev/ublk-control` is absent:

```sh
sudo modprobe ublk_drv
```

The preferred setup gives the existing `plugdev` group access to the global control node. Each created device uses `UBLK_F_UNPRIVILEGED_DEV`. The upstream owner helper then assigns `/dev/ublkcN` and `/dev/ublkbN` to the kernel-recorded owner.

`scripts/setup.sh` performs the steps in this section. To do them by hand, first clone ublksrv at the tested revision:

```sh
git clone https://github.com/ublk-org/ublksrv.git ublksrv
git -C ublksrv checkout --detach 23461b7aa314d2ee9e014356c210a5bfaa9eecec
(cd ublksrv && autoreconf -fi && ./configure)
```

Build the helper as the normal user from that checkout. It needs the `liburing-dev` headers. If the tree holds a build from another architecture, run `make -C ublksrv distclean` and `(cd ublksrv && ./configure)` first. Install the helper, library, and restrictive udev rule as an administrator:

```sh
make -C ublksrv/lib libublksrv.la
make -C ublksrv ublk_user_id
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
sudo usermod -aG plugdev "$USER"
```

The group change takes effect at the next login. Until then, run commands through `sg plugdev -c '...'`.

libublk writes device metadata to `/run/ublksrvd` and fails with `other IO failure` if it can't create it. The ext4 helper installer creates this directory. Without that installer, add the same tmpfiles rule. Use numeric IDs: the installer refuses a rule with different text.

```sh
printf 'd /run/ublksrvd 0700 %s %s -\n' "$(id -u)" "$(id -g)" | sudo tee /etc/tmpfiles.d/block-storage-ublk.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/block-storage-ublk.conf
```

After each boot, `make setup-blockcheck-host` loads `ublk_drv` through sudo and verifies these steps. It installs nothing.

The project does not install or reload these host files automatically. Do not install the vendored rule unchanged because it grants mode `0666` on `/dev/ublk-control`. Do not run Make or Cargo as root because they create root-owned build artifacts.

Override test limits with `TEST_PER_TEST_SECONDS` and `TEST_SUITE_SECONDS`.

## Safety

Automated engine and ublk lab tests use disposable regular files. Any future LVM test must identify and validate an explicitly disposable target before writing. Never use the laptop's system NVMe, a mounted filesystem or an arbitrary block device.

## Current limits

The format-4 engine has one serialized writer and 4 KiB payloads. It reclaims obsolete blocks but can still run out of usable backing space. The format-4 filesystem and SQLite clean-restart results do not establish power-loss durability. After an uncertain backing failure, the daemon stops serving I/O; the ADR-05 retry is a library opt-in only. Operator-run medium faults and the remaining ADR-06 application gates are separate work.
