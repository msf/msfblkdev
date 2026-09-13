# SQLite on ext4

SQLite 3.51.3 passed all 87 ext4 runs in 131.696 seconds on 2026-09-13, with tmpfs backing. [The audit](evidence/sqlite-ext4-audit.json) verifies 348 filesystem checks, 174 logical validations and complete cleanup. Each WAL stress run completed 12 automatic checkpoints. [The selected run](evidence/sqlite-1789293925836503146/summary.json) is the initial baseline.

This suite uses clean shutdown and restart only. It does not complete ADR-06 Delivery 3: SQLite on XFS and RocksDB remain separate work.

## Run the suite

Use the normal-user ublk setup and installed protocol-3 mount helper described in [README.md](README.md). Do not reinstall the helper for this suite. The Makefile builds the helper without the optional `lab` feature, then builds the JSON-enabled lab separately.

Build the pinned tools explicitly:

```sh
make setup-sqlite
make check-sqlite
```

The setup script verifies official source and amalgamation archive hashes. It builds adapted upstream `speedtest1`, `kvtest`, and the SQLite shell under ignored `.tools/sqlite-3.51.3/`. The manifest records source IDs, archive and adaptation hashes, compiler flags, effective compile options, and artifact hashes. `check-sqlite` verifies the existing build; it does not download or rebuild SQLite.

After operator approval for live ext4 mounts, run the full gate:

```sh
make test-sqlite-ext4
```

For a single diagnostic run:

```sh
make test-sqlite-ext4-one \
  SQLITE_CASE=kv-random-read-small \
  SQLITE_PROFILE=wal-normal \
  SQLITE_REPETITION=1
```

Selectors accept only catalog members and repetitions 1 through 3. A selected run is not full acceptance. `make test-sqlite` retains the original single WAL/FULL probe.

The suite uses one workload at a time. Each run gets a fresh 128 MiB ext4 device and a 1 GiB backing file. Defaults remain 60 seconds per child command and 600 seconds for the suite. A shared ledger limits cumulative backing writes to 512 MiB per run, including format and both daemon lifetimes. Failed runs retain their images and raw evidence.

## What runs?

Each short case runs three times under DELETE/EXTRA, WAL/FULL, and WAL/NORMAL. All 81 short runs must pass before the six WAL stress runs start.

| Case | Dataset | Operations | Transaction scope |
|---|---|---|---|
| `speedtest-main` | Upstream main test set, size 1 | 32 individually reported mixed SQL subtests | Upstream defaults |
| `kv-sequential-read-small` | 32 × 128-byte BLOBs | 4096 reads | One transaction |
| `kv-random-read-small` | 32 × 128-byte BLOBs | 4096 reads | One transaction |
| `kv-sequential-read-large` | 32 × 8192-byte BLOBs | 4096 reads | One transaction |
| `kv-random-read-large` | 32 × 8192-byte BLOBs | 4096 reads | One transaction |
| `kv-sequential-update-small-single-transaction` | 32 × 128-byte BLOBs | 256 updates | One transaction |
| `kv-sequential-update-small-per-op-commit` | 32 × 128-byte BLOBs | 256 updates | One commit per update |
| `kv-random-update-large-single-transaction` | 32 × 8192-byte BLOBs | 256 updates | One transaction |
| `kv-random-update-large-per-op-commit` | 32 × 8192-byte BLOBs | 256 updates | One commit per update |
| `kv-long-wal-checkpoint-stress` | 2 × 8192-byte BLOBs | 4096 random updates | One commit per update; WAL profiles only |

Concurrency is one. No case uses the BLOB API. The adapter applies and verifies profiles on the actual benchmark connections: 4096-byte pages, 256 cache pages, mmap disabled, and the default 1000-frame automatic-checkpoint threshold. WAL stress requires at least two successful automatic PASSIVE checkpoints. Final WAL drain requires a checked TRUNCATE checkpoint and successful close.

`kvtest` keeps its upstream access-order PRNG. Its SQL `randomblob()` uses `splitmix64-byte-v1`, seeded with `0x6a09e667f3bcc909`, so independent reference runs have identical contents. `speedtest1` keeps its upstream deterministic generator. Its verification work remains inside its reported workload time.

Database, journal, WAL, shared-memory, and SQLite temporary files stay under the mounted workload directory. Reference databases use a separate owned directory outside the device. Each run requires `integrity_check = ok`, an empty `foreign_key_check`, and the reference `.sha3sum --schema` digest. `speedtest1` also checks its upstream verification hash. The lab repeats logical verification after a clean daemon restart and remount, without repopulation. Four unmounted `e2fsck -f -n` checks cover each lifecycle.

## How are measurements interpreted?

Each case directory contains `commands.log` and one schema-version-2 `result.jsonl`. The run directory contains `manifest.json` and `summary.json`. The manifest preserves attempted, completed, failed, and timed-out state. The summary reports all three samples, their median, and their range for each case/profile pair.

KV initialization, workload, drain/close, validation, clean restart/remount, reopen/revalidation, cleanup, and full-case elapsed time are separate. `speedtest1` creates its schema and data inside its mixed workload; a separate initialization time is unavailable, not zero. Its subtest times are preserved, but an aggregate transaction count and transaction rate are unavailable.

Workload and drain durations use the tool's monotonic clock. Read-only observers capture device and ledger counters at the final drain boundaries. Their measured overhead is subtracted from process duration. The parent process envelope is retained separately because its polling quantizes elapsed time. WAL workload-plus-drain includes final checkpoint completion.

I/O records distinguish workload, final drain, surrounding residual I/O, and final shutdown totals. Data requests come from kernel sectors divided by eight: this device has verified 4096-byte logical and maximum request sizes. Kernel write-completion counts also include zero-data flushes and cannot directly count data writes. Final daemon reports account for completions after the last kernel sample.

Backing bytes are actual engine requests, not host-filesystem or physical-device write amplification. The ledger reserves writes before submission and rejects exhausted, dirty, or incomplete accounting. Its unsafe mmap API requires exclusive ownership of a private file and no external mutation or truncation while mappings exist.

KV reads are warm from initialization. `speedtest1` starts with a fresh database and warms caches during its mixed workload. No global caches are changed. Zero observed device reads do not support device-read performance claims. Upstream tools do not provide tail-latency percentiles for these cases. Three short samples do not establish sustainable throughput or a small performance improvement. Tmpfs backing does not establish physical-media durability.

## Select and compare a baseline

Only a complete correctness-passing 87-run matrix can become a baseline. The repository already contains `evidence/sqlite-ext4-baseline.json`. To select another run, replace the example run path and choose a new output filename. Selection refuses to overwrite an existing file:

```sh
uv run --quiet python tools/sqlite/baseline.py select \
  evidence/sqlite-RUN evidence/new-sqlite-baseline.json
```

To compare a later complete run, replace the candidate path with its evidence directory:

```sh
uv run --quiet python tools/sqlite/baseline.py compare \
  evidence/sqlite-ext4-baseline.json evidence/sqlite-CANDIDATE
```

Comparisons require matching workloads, settings, SQLite build, hardware, kernel, backing geometry, medium, and cache conditions. An intentional block-device commit change requires `--allow-commit-change`. Database or benchmark upgrades require a new baseline. Artifact hashes detect changes to the selected results.

The report includes percentage changes in median workload time, workload-plus-drain time, throughput, and available tail latency. It marks overlapping ranges inconclusive and applies no regression threshold. A self-comparison is explicitly labeled; its zero changes are not an independent performance result. Rerun apparent changes that overlap observed variation, preferably with interleaved baseline and candidate runs.

Run the non-root baseline parser tests without SQLite or mounts:

```sh
uv run --quiet python tools/sqlite/test_baseline.py
```

## Upstream multiprocess audit

The pinned source archive contains `mptest/mptest.c`. We inspected its child launch and connection setup, but did not add a concurrency workload. Its default disables synchronization at line 1385. `--sync` prevents that override and propagates to clients at lines 648–650. On Unix it launches background clients through `system()` at lines 655–657. Future integration must retain synchronization and own the full process group under the existing deadlines. The source also enables extension loading, which the current lab build omits.

Audited file SHA-256: `e8d5b5bf2a84d428b0115283383a9a42c865d3b65ca7aae35241a8f124448913`. This is a source audit, not a multiprocess test result.

## Remaining scope

SQLite on XFS, RocksDB, concurrent database clients, crash injection, and power-loss durability are not accepted by this suite. Keep ADR-06 Delivery 3 open until its remaining database/filesystem combinations pass.
