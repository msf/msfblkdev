# v1.1 versus original v1.2: performance before the batching fix

Follow-up: [v1.2.1 packed writes and compatible cache sizing](RESULTS-PERF-PACKED-WRITES.md)
removes the record split and repeats these baselines. This report contains only the
original, unfixed v1.2 results. It contains no measurements of the fixed implementation.
Its conclusions describe performance before the batching fix.

Measured 2026-09-19. The default cache preserves performance for the tested local reads.
Widely scattered reads need a cache near the old dense map's size. No tested cache
restores v1.1 performance for scattered batches of writes.
The follow-up ublk/fio comparison confirms the throughput pattern. A complete mapping
cache still leaves a measurable scattered-read tail-latency gap through the frontend.

## KPIs and workloads

Use four primary KPIs: completed IOPS (or MiB/s), p99 read latency, p99 flush latency,
and process RSS. Record cache hit rate, mapping reads, pressure checkpoints, and
payloads per data record to explain changes. Buffered-write call latency alone is
misleading: most work occurs during flush. Record clean-open and close time separately.

These are direct engine API measurements, not ublk/fio or filesystem/database results.
Both exact release tags use the same driver, release compiler, single caller, 4 KiB
operations, 4 MiB write buffer, and 64 MiB checkpoint threshold. The only adapter
differences configure v1.2's cache and expose its statistics.

| Workload | Bounded operation sequence |
| --- | --- |
| Sequential reads | 32,768 reads over a populated 64 MiB range, after one warmup pass |
| Hot reads | 32,768 seeded random reads of 32 blocks, one per mapping page; 128 KiB of payload across 40 MiB of logical address space |
| Wide reads | 32,768 seeded random reads of 3,277 blocks, one per mapping page across the full 4 GiB logical volume; about 12.8 MiB of payload |
| Sequential writes | Write 64 MiB, flush every 256 blocks (1 MiB) |
| Wide writes | 4,096 seeded random writes to the wide-read address set, flush every 32 writes (128 KiB); includes repeated addresses |

Reads start after warming the address set, not after dropping host caches. A cache too
small for the set still misses during measurement. Write runs start from a fresh image.
Write throughput includes all periodic flushes; final checkpoint/close is reported separately.
Reads validate the returned address marker; the engine also validates payload checksums.
Written addresses are checked after close/reopen, outside the timed interval.

Every run uses a fresh 1 GiB regular-file backing for a 4 GiB logical volume. The maximum
filesystem allocation observed was 93,933,568 bytes (89.58 MiB). Runs cannot accumulate
an unbounded overwrite log. Logical address coverage and append-log consumption are different:
the wide workload touches the whole logical range without filling the backing.

## Measured comparison

Medians of three interleaved repetitions. IOPS below are thousands of 4 KiB operations/s;
multiply by 3.90625 for MiB/s. Read p99 and flush p99 are medians of per-run percentiles.

| Workload | v1.1 | v1.2 default | v1.2 full mapping cache |
| --- | ---: | ---: | ---: |
| Sequential reads, kIOPS | 46.0 | 46.0 | 45.5 |
| Hot reads, kIOPS | 29.7 | 32.9 | 32.5 |
| Wide reads, kIOPS | 19.1 | 9.4 | 18.6 |
| Sequential writes, kIOPS | 231.5 | 222.7 | 263.5 |
| Wide writes, kIOPS | 57.0 | 9.3 | 23.3 |
| Wide-read p99, microseconds | 80.35 | 162.27 | 81.32 |
| Wide-write flush p99, milliseconds | 0.73 | 7.80 | 1.90 |
| Wide-read process RSS, MiB | 18.92 | 7.32 | 18.96 |

Sequential writes were noisy. The original v1.1 runs ranged from 67.7 to 238.8 kIOPS.
A separate five-repetition control measured medians of 237.8 / 248.8 / 270.5 kIOPS
for v1.1 / default / full cache. Its respective ranges were 126.8–255.9,
222.7–293.3, and 223.5–271.7 kIOPS. This supports no consistent sequential-write
regression, not a precise speedup claim. The read and wide-write results were much tighter;
for example, wide reads ranged from 19.04–19.14 kIOPS on v1.1 and 18.42–18.57 with the full cache.

## Which cache matches v1.1?

Use “at least 90% of baseline throughput” as a practical first threshold, and check p99
separately. Three short repetitions are an initial comparison, not a statistical equivalence test.

| Requested v1.2 cache | Slots | Wide-read hit rate | Wide-read kIOPS | Read p99, microseconds | Process RSS, MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Default: 429,496 bytes | 111 | 3.59% | 9.36 | 162.27 | 7.32 |
| 1 MiB | 270 | 8.47% | 9.53 | 164.58 | 7.92 |
| 4 MiB | 1,080 | 33.31% | 11.01 | 156.07 | 10.89 |
| 12 MiB | 3,250 | 99.25% | 18.47 | 103.71 | 18.86 |
| 16 MiB, capped to the complete map | 3,277 | 100% | 18.57 | 81.32 | 18.96 |

For this 4 GiB geometry, `--mapping-cache-bytes 16777216` restores wide-read throughput
to 97.4% of v1.1 and p99 to within 1.2%. The cache actually allocates **12,684,056 bytes
(12.10 MiB)** because capacity is capped at all 3,277 pages; the resident index adds
13,108 bytes. v1.1's dense map occupies 12,582,912 bytes (12 MiB). This setting restores
the old memory footprint as well as read performance. It is not a memory saving for this workload.

The 12 MiB setting meets the throughput threshold but leaves p99 about 29% worse.
The default already matches the tested sequential and hot reads, with approximately
61% lower process RSS and 96.5% less mapping storage. Its 111 slots cover 138.75 MiB
of contiguous logical space. Sparse addresses need sizing by **distinct mapping pages**,
not by live payload bytes: about 12.8 MiB of scattered data needed the whole map here.

A useful sizing estimate is about 3.9 KiB per active mapping page, plus the separate
resident index. The byte-exact result comes from `mapping_stats()`, including hash-table
capacity steps. This estimate predicts capacity, not performance on another workload.

## Why more cache does not fix the wide writes

The same write sequence releases 4,080 payloads after coalescing duplicates within each
flush window. v1.1 packs them into **128 data records**; v1.2 emits **4,080 records**.
The v1.2 release rule groups by mapping page, and these addresses usually occupy different pages.

| v1.2 cache request | Pressure checkpoints | Wide-write kIOPS | Flush p99, milliseconds |
| --- | ---: | ---: | ---: |
| Default | 36 | 9.30 | 7.80 |
| 1 MiB | 14 | 10.41 | 11.23 |
| 4 MiB | 3 | 13.91 | 28.03 |
| 12 MiB | 0 | 22.74 | 2.07 |
| 16 MiB | 0 | 23.26 | 1.90 |

More cache removes pressure checkpoints, but it cannot change the data-record count.
The full-cache run has no mapping reads or pressure checkpoints during the measured writes,
yet achieves only 40.8% of v1.1 throughput. The record splitting is a concrete remaining
structural cost; attributing every remaining microsecond would require profiling.

Fewer, larger pressure checkpoints can worsen tail latency: 4 MiB improves throughput
over the default but raises flush p99 to 28 ms. Cache hit rate alone misses this tradeoff.
There are only 64 flush samples per sequential-write run and 128 per wide-write run;
their p99 values are coarse estimates near the worst observed flush.

Close also matters. Wide-write close takes 12.35 ms on v1.1, 2.23 ms with the default cache,
and 58.25 ms with the full cache. Including close lowers throughput to 48.46 / 9.24 /
17.59 kIOPS respectively. Large-cache checkpoint work is deferred, not eliminated.
Clean open of the wide-read fixture takes 23.36 ms on v1.1 versus 113.36–127.85 ms for
v1.2 default/full: v1.2 validates all referenced mapping snapshots regardless of cache size.

## Does the difference reach the ublk frontend?

Yes. A follow-up ran 36 fio cases: four workloads, three configurations, and three
interleaved repetitions. These use the same address sequences and flush intervals as
the engine comparison. Each case gets a fresh backing. fio replays 4 KiB operations
with `psync`, queue depth 1, and direct I/O. The release daemon runs on CPU 0 and fio
on CPU 1. No filesystem is mounted on the device.

For read cases, fio populates the address set, the daemon closes and restarts, and fio
warms the set before measurement. For write cases, a separate daemon restart and read
pass verify every written address. fio checks CRC32C headers and payloads. A negative
control confirmed that the same replay/verification options reject a corrupted payload.
The control used a disposable regular file with buffered I/O; benchmark device I/O is direct.

| Metric | v1.1 | v1.2 default | v1.2 full mapping cache |
| --- | ---: | ---: | ---: |
| Sequential reads, kIOPS | 36.13 | 34.46 | 34.49 |
| Wide reads, kIOPS | 16.93 | 8.74 | 16.02 |
| Sequential writes, kIOPS | 73.80 | 84.45 | 81.92 |
| Wide writes, kIOPS | 27.49 | 7.41 | 17.81 |
| Wide-read completion p99, microseconds | 73.22 | 158.72 | 90.62 |
| Wide-write flush p99, milliseconds | 1.89 | 15.01 | 2.61 |
| Wide-read daemon RSS, MiB | 20.05 | 8.59 | 20.25 |

All entries are medians. fio write throughput includes the replayed flushes and excludes
daemon close. Read tails use fio's completion-latency histogram. RSS covers the daemon,
not fio, and is sampled after read warmup or before the write workload.

With the full cache, wide-read throughput reaches **94.7%** of v1.1, but p99 remains
**23.8% higher**. The engine-only p99 match does not establish an end-to-end match.
The full cache restores **64.8%** of wide-write throughput; the default restores 26.9%.
Wide-read throughput ranges were tight: 16.91–16.93 / 8.21–8.80 / 15.87–16.21 kIOPS.
Wide writes varied more: 24.82–32.51 / 7.21–8.03 / 15.23–19.05 kIOPS, respectively.
The slowest v1.1 sequential-write repetition reached 42.78 kIOPS, so the apparent
sequential-write speedup needs a longer controlled experiment before it becomes a claim.

All 36 cases completed their expected operation and flush counts, data checks, and
graceful device removal. No benchmark device or backing remained. Maximum backing
allocation was again 89.58 MiB. The full fio/daemon outputs are retained in the raw archive.

## Which backing operations explain the write cost?

A separate 15-case diagnostic uses the existing accounting API to count backing I/O
between open and close. These counts include checkpoint work triggered during writes,
but exclude format, open, and final close. Accounting is enabled only in these diagnostic
runs; their timings are not used as performance results.

For 4,096 scattered writes with a flush every 32 writes:

| Count | v1.1 | v1.2 default | v1.2 full mapping cache |
| --- | ---: | ---: | ---: |
| Released payloads | 4,080 | 4,080 | 4,080 |
| Backing read operations | 0 | 1,586 | 0 |
| Backing write operations | 128 | 8,256 | 4,080 |
| Backing flush operations | 128 | 236 | 128 |
| Backing bytes written | 17,235,968 | 50,528,256 | 33,423,360 |

With a full cache, the record split means **31.9 times as many backing writes** and
**1.94 times as many bytes written**, despite identical flush counts. The implementation
submits and waits for each record separately. This is a directly measured submission
and byte cost, not a cache-hit-rate explanation. It does not attribute all elapsed time.

The flush-interval controls support that explanation. Flushing every write produces
4,096 backing writes and 4,096 flushes in both v1.1 and full-cache v1.2. Flushing every
256 writes produces 16 versus 3,936 backing writes, with 16 flushes each. At 1,024,
the counts are 12 versus 3,504 backing writes and four flushes each. More buffering
lets v1.1 combine unrelated mappings; v1.2 retains the one-mapping-page record rule.

Even a 32-page working set that fits the default cache produces 128 versus 2,568 backing
writes at the 32-write flush interval, with no metadata reads in either version.
This isolates the record-grouping cost from cache pressure.

## Reproduce and retain the KPIs

Host: AMD Ryzen AI 9 HX 370, Linux 7.0.0-31-generic, Rust 1.97.1. Each timed process
was pinned to CPU 0. Frequency policy and other host activity were not controlled.
Backings live on the home Btrfs filesystem, not the RAM-backed `/tmp`. The private fixture
directory uses `chattr +C`; engine backing I/O uses `O_DIRECT`. Build artifacts use `/tmp`.
RSS is sampled from `smaps_rollup` after open/read warmup and before latency-vector allocation;
it includes process/allocator overhead and is not a peak-memory measurement.

```sh
uv run --quiet python tools/mapping-bench/run.py \
  --repetitions 3 --output evidence/mapping-bench-new.jsonl
uv run --quiet python tools/mapping-bench/summarize.py evidence/mapping-bench-new.jsonl
```

The runner archives the tags without changing the current checkout, copies the common
driver plus API adapter, and builds/checks each example with release-mode Cargo and Clippy.
It requires Linux, working `io_uring`, `taskset`, and a Btrfs fixture directory supporting
`chattr +C`. It creates no block device or mount. It removes each backing after verification.

Sources: [driver](tools/mapping-bench/compare.rs), [runner](tools/mapping-bench/run.py),
[summary tool](tools/mapping-bench/summarize.py). The driver SHA-256 is
`a1cd41b50f096853fa421c85dcc5bf0894f169e9abaec8cf875a2b1bbb0c097d`.
Tags: v1.1 `1ea859783621dfad36cc75cd5616a8613ae2d9c7`, v1.2 `4b77c4daf79560a437954743fd7c2a5bf904312f`.

Raw evidence:

- [Main sweep](evidence/mapping-bench-v11-v12-2026-09-19.jsonl): 90 runs, SHA-256 `2c604b061e200f74056876689ed874c4b8a265f18a2348886293338fe12801f3`.
- [Sequential-write control](evidence/mapping-bench-seq-write-control-2026-09-19.jsonl): 15 runs, SHA-256 `123576ab4ea56fe44459e8fb26ed62cc1aae1ffca37200d7bafbe18cc933ce24`.

Run the ublk comparison with the existing operator-authorized disposable-device setup:

```sh
uv run --quiet python tools/mapping-bench/ublk.py \
  --build-dir /tmp/msfblkdev-perf-build --output evidence/mapping-ublk-new
uv run --quiet python tools/mapping-bench/summarize_ublk.py evidence/mapping-ublk-new
uv run --quiet python tools/mapping-bench/io_counts.py \
  --build-dir /tmp/msfblkdev-perf-build --output evidence/mapping-io-counts-new.jsonl
```

The ublk runner builds both tagged release daemons. It requires accessible ublk nodes
and fio. It validates device identity and geometry before I/O, requests graceful shutdown,
and checks device removal. On failure, it preserves the private fixture for investigation.
Successful runs retain measured JSON and archive raw daemon/fio logs. No root helper
replacement, filesystem mount, or storage-engine change is involved.

Follow-up evidence:

- [fio measurements](evidence/mapping-ublk-2026-09-19/results.jsonl): SHA-256 `c33991706a8be398f9a8f790950521084adbd8fc9fe006510e209c0a74be31e4`.
- [Build/environment metadata](evidence/mapping-ublk-2026-09-19/metadata.json) and [raw logs](evidence/mapping-ublk-2026-09-19/raw-logs.tar.gz): archive SHA-256 `2cca3fd26694631c931cdc745b05e462f1ca7f5f0a1d93df78c14e1614504d04`.
- [Backing-I/O diagnostic](evidence/mapping-io-counts-2026-09-19.jsonl): SHA-256 `7a6a865c5aa756100a076b5d7e21a215fd4043e787f57b872d227d1edfb75211`.
- [Corrupted-payload verification control](evidence/mapping-fio-verification-control-2026-09-19.json).

Keep the bounded engine and fio workloads as the initial performance gates. Compare
throughput and p99 separately; use a provisional 90% throughput floor and 10% p99
increase ceiling, with RSS reported alongside them. Under that criterion, the full cache
passes wide-read throughput but fails its fio p99 comparison, and no cache passes wide writes.

The next optimization investigation should target record submission overhead and footer
bytes, while preserving ADR-07's recovery rules. Merely increasing the cache or flush
batch does not restore v1.1's batching efficiency. SQLite application measurements remain
open; these results cover the engine and raw block-device paths.
