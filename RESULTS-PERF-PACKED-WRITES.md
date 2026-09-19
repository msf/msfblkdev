# v1.2.1: packed-write performance against v1.1 and original v1.2

Measured 2026-09-19. Sorting the whole write buffer and packing across mapping-page
boundaries restores the scattered-write record count from 4,080 to 128. With a full
mapping cache, fio write throughput reaches 34.1k IOPS, versus 33.6k on v1.1 and
14.8k on v1.2 in this rerun. Flush tails and final checkpoint cost still exceed v1.1.

These are fresh measurements of v1.1, original v1.2, and the corrected v1.2.1 implementation.
The [original comparison](RESULTS-PERF-PAGED-MAPPING.md) remains the record of the
earlier experiment. The v1.2 tag retains the original implementation; v1.2.1 contains the fix.

| Label used in this report | Implementation | Raw evidence label |
| --- | --- | --- |
| v1.1 | Original dense mapping | `v1.1` |
| v1.2 original | Unfixed release: one data record per mapping page | `v1.2` |
| v1.2.1 | Corrected release: sorted buffer, packed records across mapping pages | `working` |

Every mention of v1.2 refers to the unfixed release. The tables compare it with v1.2.1;
no fixed result is labelled v1.2. The fixed code was benchmarked before committing, so
its raw evidence retains the label `working`. Baseline numbers differ from the earlier report because
both unchanged tags were measured again in this experiment.

## What changed?

The writer sorts buffered LBAs, retains their payload arena locations, and packs up
to 338 payloads per record. Exact record-write completion precedes mapping updates.
One data-log fsync completes the flushed group. A required checkpoint occurs before
the group and retains its separate durability barriers. The disk format is unchanged.

The cache must hold `min(effective_buffer_payload_slots, volume_mapping_pages)` pages.
This covers a full buffer with every payload on a different mapping page. Open rejects
an explicit smaller cache before recovery writes; automatic sizing includes this floor.
The checkpoint interval must also accommodate one complete packed buffer.

| Configuration, 4 GiB logical volume and 4 MiB write buffer | Mapping slots | Allocated cache bytes |
| --- | ---: | ---: |
| v1.1 dense mapping | All LBAs | 12,582,912 |
| v1.2 original automatic cache | 111 | 429,464 |
| v1.2.1 automatic cache | 1,024 | 3,977,360 |
| Explicit 4 MiB cache, either paged implementation | 1,080 | 4,192,856 |
| Explicit 16 MiB cache, capped to the complete map | 3,277 | 12,684,056 |

The paged implementations also allocate a 13,108-byte resident index. Cache allocation
excludes payload buffers, candidate indexes, allocator overhead, and other process memory.
The automatic cache therefore changes from 0.41 MiB to **3.79 MiB** for this configuration.
Comparing the two defaults alone would mix batching gains with a larger cache.

## What reaches the block-device frontend?

Medians of three interleaved repetitions. Throughput includes periodic flushes and
excludes daemon close. kIOPS means thousands of 4 KiB operations/s; multiply by
3.90625 for MiB/s. Percentiles are medians of each run's percentile.

| fio metric | v1.1 | v1.2 original, full cache | v1.2.1, full cache | v1.2.1, automatic cache |
| --- | ---: | ---: | ---: | ---: |
| Sequential reads, kIOPS | 34.13 | 33.85 | 34.21 | 35.16 |
| Scattered reads, kIOPS | 16.47 | 16.38 | 16.84 | 10.06 |
| Sequential writes, kIOPS | 81.11 | 78.77 | 88.09 | 78.39 |
| Scattered writes, kIOPS | 33.57 | 14.79 | 34.13 | 16.13 |
| Scattered-read completion p99, µs | 95.74 | 92.67 | 91.65 | 168.96 |
| Scattered-write flush p99, ms | 0.91 | 3.06 | 3.78 | 27.66 |
| Scattered-read daemon RSS, MiB | 20.18 | 20.25 | 20.28 | 12.00 |

Both paged implementations' full caches allocate **12.10 MiB**, approximately the old dense mapping footprint.
The fixed implementation reaches 102% of baseline scattered-read throughput and 102% of scattered-write
throughput in this rerun. That meets the provisional 90% throughput floor. Read p99
also meets the provisional 10% increase ceiling; flush p99 does not.

Scattered-write throughput ranges were 31.75–35.62k IOPS for v1.1,
13.56–18.04k for v1.2 original full cache, and 31.03–36.90k for the fixed full cache.
These results support restored throughput, not a precise speedup over v1.1.
Sequential writes also vary: the fixed automatic-cache runs span 56.50–91.02k IOPS.

The earlier report's 24% read-p99 regression with a full cache did not recur even in
the unchanged v1.2 baseline. Do not attribute that difference to the batching patch.
The host's frequency policy and background activity are uncontrolled, and three short
repetitions do not establish statistical equivalence. The current flush-p99 difference
also needs a longer, targeted experiment before assigning a cause.

The automatic-cache fio comparison improves from v1.2's 7.21k to the fixed implementation's 16.13k scattered-write
IOPS, but flush p99 increases from 12.65 to 27.66 ms. This setting saves memory and
still falls well short of baseline performance for a widely scattered working set.

## What does the engine show at equal cache budgets?

The direct API benchmark isolates engine work from ublk and fio. Each scattered-write
run submits 4,096 writes and flushes every 32 writes. Coalescing leaves 4,080 payloads.

| Scattered-write metric | v1.1 | v1.2 original, full | v1.2.1, full | v1.2 original, 4 MiB | v1.2.1, 4 MiB | v1.2.1, automatic |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Throughput, kIOPS | 61.13 | 23.59 | 55.38 | 13.97 | 20.67 | 21.23 |
| Flush p99, ms | 0.73 | 2.80 | 0.85 | 28.71 | 27.44 | 25.73 |
| Data records | 128 | 4,080 | 128 | 4,080 | 128 | 128 |
| Pressure checkpoints | 0 | 0 | 0 | 3 | 3 | 3 |
| Final close, ms | 12.04 | 57.24 | 55.76 | 5.74 | 6.45 | 10.53 |
| Throughput including close, kIOPS | 51.63 | 17.48 | 31.63 | 13.72 | 20.06 | 20.13 |

At the same full-cache allocation, packing raises engine throughput by **2.35×** and
reaches **90.6% of v1.1**. At the same explicit 4 MiB budget, it raises throughput by
48%. The unchanged checkpoint count at each budget separates the record-packing gain
from cache-size effects. Exact I/O-accounting tests additionally verify one write per
packed record and one data-log fsync across 338, 339, 1,024, and 1,500 scattered payloads.

Final close remains material: fixed full-cache throughput including close is only **61.3% of
v1.1**. The fixed engine retains the cost of writing dirty mapping snapshots and
publishing a checkpoint. Packing data records does not remove that work.
The fixed automatic cache triggers three larger pressure checkpoints versus 36 with v1.2's
smaller default. Fewer checkpoints improve throughput but can worsen individual flushes.

| Other engine metric | v1.1 | v1.2 original, full | v1.2.1, full | v1.2.1, automatic |
| --- | ---: | ---: | ---: | ---: |
| Sequential reads, kIOPS | 46.16 | 46.67 | 46.46 | 47.31 |
| Hot reads, kIOPS | 31.81 | 32.88 | 32.77 | 33.15 |
| Scattered reads, kIOPS | 19.50 | 19.00 | 19.42 | 11.12 |
| Scattered-read p99, µs | 75.25 | 79.26 | 75.97 | 154.88 |
| Sequential writes, kIOPS | 235.27 | 233.77 | 280.48 | 280.92 |
| Sequential-write data records | 64 | 103 | 64 | 64 |
| Scattered-read clean open, ms | 24.03 | 131.79 | 132.71 | 129.74 |

The fixed full cache has a 100% scattered-read hit rate; its automatic cache has
31.65%. An explicit 4 MiB cache has 33.31% and reaches 11.19k IOPS. Sparse live payloads
therefore still need a cache sized by their distinct mapping pages. Startup continues
to validate all referenced snapshots, and its cost is essentially unchanged from v1.2.

## How were the runs bounded and verified?

The unchanged common driver uses a 4 GiB logical volume, fresh 1 GiB regular-file
backing, 4 MiB write buffer, and 64 MiB checkpoint interval. Sequential writes cover
64 MiB and flush every 256 blocks; scattered writes cover 4,096 seeded addresses and
flush every 32. Reads perform 32,768 operations after warmup. The scattered set contains
3,277 blocks, one per mapping page; the hot set contains 32. Read payloads and written
data after close/reopen are verified. No workload relies on space reclamation.

The sweep completed **105 engine cases and 60 fio cases**. All expected operation and
flush counts passed. All 150 archived fio setup, warmup, measured, and verification jobs
reported zero errors. Maximum backing allocation was 93,933,568 bytes (89.58 MiB).

Host: AMD Ryzen AI 9 HX 370, Linux 7.0.0-31-generic, Rust 1.97.1, fio 3.41.
Release engine/daemon processes run on CPU 0; fio runs on CPU 1 with `psync`, queue
depth 1, direct I/O, and CRC32C verification. Private backings use Btrfs with `chattr +C`;
builds use `/tmp`. Engine and fio measurements ran separately from correctness gates.
RSS is a sampled process value, not a peak. Read runs retain warmed host caches.

There are only 64 sequential and 128 scattered flush samples per run. Their p99 values
are coarse estimates near the slowest flush. Retain throughput, read p99, flush p99,
RSS, record counts, pressure checkpoints, open time, and close time as separate KPIs.
Use the 90% throughput floor and 10% p99 ceiling as review triggers, not automatic CI
failures on this uncontrolled host. No tested setting matches v1.1 across every KPI.

## Reproduce and retain the evidence

The runners preserve both release tags and copy the current Rust sources into a private
build tree for `--working-tree`. The 1 MiB candidate setting is omitted because it is
incompatible with this driver's 4 MiB buffer. Use a new output path for each run.
The ublk command uses the operator-authorized disposable-device fixtures.

```sh
uv run --quiet python tools/mapping-bench/run.py --working-tree \
  --build-dir /tmp/msfblkdev-packed-perf --budgets default 4194304 16777216 \
  --repetitions 3 --output evidence/mapping-bench-next.jsonl
uv run --quiet python tools/mapping-bench/summarize.py evidence/mapping-bench-next.jsonl
uv run --quiet python tools/mapping-bench/ublk.py --working-tree \
  --build-dir /tmp/msfblkdev-packed-perf --output evidence/mapping-ublk-next
uv run --quiet python tools/mapping-bench/summarize_ublk.py evidence/mapping-ublk-next
```

Tags: v1.1 `1ea859783621dfad36cc75cd5616a8613ae2d9c7`,
v1.2 `4b77c4daf79560a437954743fd7c2a5bf904312f`.
The v1.2.1 engine change is commit `8f2b2ae`; its Rust sources match the measured build.
Both pre-release v1.2.1 builds used source SHA-256
`77cb7560815cab83c2c3c1f90123546401720ae7d5ea400ad1da2c2847a16b5b`.
This hashes sorted relative paths and contents of the copied Rust tree, including the
common driver and adapter. The driver itself is unchanged from the original comparison.

- [Engine measurements](evidence/mapping-bench-packed-2026-09-19.jsonl): SHA-256 `68a76e55992a047fd574c8b278d3ac8bfc83f038858819ee19faa169572728aa`.
- [fio measurements](evidence/mapping-ublk-packed-2026-09-19/results.jsonl): SHA-256 `56e7a9fc789c262e6c674925db8479b33b2651cc1ba17f8b2e86e518e93ce253`.
- [Build metadata](evidence/mapping-ublk-packed-2026-09-19/metadata.json) retains binary hashes and configurations.
- [Raw fio and daemon logs](evidence/mapping-ublk-packed-2026-09-19/raw-logs.tar.gz): SHA-256 `bbde4acaa7e7304db16bed1ec2b16db7d6b1e4f735bdb156c60c56e6895fd5e3`.

The remaining performance work is checkpoint/close cost and controlled flush-tail
measurement. The filesystem and SQLite runs remain correctness gates; this report
does not claim database transaction throughput or sustained overwrite performance.
