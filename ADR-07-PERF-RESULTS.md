# ADR-07 performance results

## v1.2.3: 336-entry pages and adjacent-page prefetch (2026-09-19)

The combined change improves sequential scans across mapping pages by **45–51%**
against v1.2.2 at equal cache byte budgets. Metadata read submissions fall about **76%**.
Random-read throughput remains within 0.5% of baseline after adding a sequential-access
trigger. Ordinary sequential payload reads show mixed results; this is not a general
read-throughput or tail-latency improvement claim.

Format **3** now contains 336 entries per MappingPage. The user explicitly permits
incompatible prototype refinements within the same version. Images from earlier
320-entry releases are not supported or migrated; this experiment formats fresh images.
The earlier release tags and their historical measurements remain unchanged.

### What did we test before accepting the change?

The predeclared primary test was a sequential scan crossing mapping-page boundaries
with a cache smaller than the working set, measured both after reopen and after filling
the cache. Seeded random reads were the regression control; ordinary sequential reads
checked how the change affected a workload dominated by payload I/O. The initial review
trigger was a >10% random-read throughput or P99 regression.

Both versions run the same driver and identical LBAs. Each fresh fixture has a 4 GiB
logical volume, 1 GiB backing, a 256 KiB write buffer, and a 64 MiB checkpoint interval.
Population writes one payload every 320 LBAs across the volume, in shuffled order, then
a contiguous 64 MiB payload range. The fixed 320-LBA stride is deliberate: changing it
with the format would compare different logical workloads. All payloads are verified
in full, including during timed reads. No workload depends on space reclamation.

| Workload | Timed operations | Initial mapping-cache state |
| --- | ---: | --- |
| `page-cold` | 3,277 ascending reads at 320-LBA intervals | Empty after reopen |
| `page-steady` | Three ascending passes, 9,831 reads | Filled by one preceding ascending pass |
| `random` | 32,768 seeded random reads from that same address set | Filled by one ascending pass |
| `seq-blocks` | 16,384 consecutive payload reads (64 MiB) | Empty after reopen |

The final sweep contains **48 cases**: two versions, two cache budgets, four workloads,
and three interleaved repetitions. Engine processes run on CPU 0 with O_DIRECT and
private Btrfs backings using `chattr +C`. Host filesystem caches remain warm; “cold”
refers to our mapping cache. Format, population, open, and close are excluded from read
timing. Maximum backing allocation was 96,694,272 bytes (92.21 MiB).

The host is the same Ryzen AI 9 HX 370 / Linux 7.0.0-31-generic / Rust 1.97.1 system
as the earlier experiments. Frequency and background activity remain uncontrolled.
Performance measurements ran separately from correctness gates.

### What improved at equal cache budgets?

Medians of three runs. kIOPS means thousands of 4 KiB payload operations per second.
P99 is the median of each run's operation P99, including payload verification.

| Cache request | Workload | v1.2.2 kIOPS | v1.2.3 kIOPS | Change | v1.2.2 P99, µs | v1.2.3 P99, µs |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 512 KiB | Cold page scan | 9.69 | 14.14 | +46.0% | 186.99 | 161.72 |
| 512 KiB | Full-cache page scan | 9.48 | 14.33 | +51.2% | 158.02 | 159.15 |
| 512 KiB | Random reads | 9.43 | 9.38 | −0.5% | 152.22 | 154.06 |
| 512 KiB | Sequential payloads | 41.00 | 43.53 | +6.2% | 77.27 | 43.92 |
| 4 MiB | Cold page scan | 9.36 | 14.13 | +50.9% | 205.14 | 158.49 |
| 4 MiB | Full-cache page scan | 9.75 | 14.11 | +44.8% | 152.15 | 155.39 |
| 4 MiB | Random reads | 10.98 | 11.02 | +0.4% | 145.33 | 148.25 |
| 4 MiB | Sequential payloads | 43.05 | 41.35 | −3.9% | 41.65 | 59.00 |

“Full-cache page scan” means the cache has filled, not that it holds the whole map.
The steady scan's throughput improvement does not imply lower P99: triggering requests
still wait for a larger metadata read. The benefit comes from subsequent cache hits.
Random-read P99 rises 1.2% and 2.0%, respectively, below the predeclared review trigger.

Sequential payload results are mixed. At 4 MiB the candidate's P99 spans 36.55–75.03 µs
and baseline spans 38.54–43.27 µs; their throughput ranges overlap. At 512 KiB the direction
reverses. These short runs do not establish an ordinary sequential-read improvement,
and the 4 MiB tail result remains a reason for a longer controlled latency test.

| Requested cache | v1.2.2 allocated bytes / slots | v1.2.3 allocated bytes / slots |
| --- | ---: | ---: |
| 512 KiB | 524,128 / 135 | 522,272 / 128 |
| 4 MiB | 4,192,856 / 1,080 | 4,194,248 / 1,028 |

The candidate uses larger entry arrays and an accounted four-byte sorting scratch entry
per slot. It therefore holds fewer pages at the same budget. The resident index shrinks
from 13,108 to 12,484 bytes. Fixed 16 KiB read scratch lies outside the cache budget.
Sampled RSS is similar: random reads at 4 MiB use 7.29 versus 7.31 MiB. RSS is not a peak.

### Do the I/O counters support the hypothesis?

Counts are identical across repetitions. These are foreground mapping reads, excluding
open-time checkpoint validation and payload reads.

| Workload / budget | v1.2.2 submissions / pages | v1.2.3 submissions / pages | Prefetched / used / evicted unused |
| --- | ---: | ---: | ---: |
| Cold page scan, either budget | 3,277 / 3,277 | 783 / 3,121 | 2,338 / 2,338 / 0 |
| Full-cache page scan, either budget | 9,831 / 9,831 | 2,348 / 9,363 | 7,015 / 7,015 / 0 |
| Random, 512 KiB | 31,339 / 31,339 | 31,305 / 31,335 | 30 / 1 / 29 |
| Random, 4 MiB | 21,852 / 21,852 | 21,563 / 21,574 | 11 / 3 / 8 |
| Sequential payloads, either budget | 52 / 52 | 14 / 49 | 35 / 35 / 0 |

Each mapping page contributes 4,096 metadata bytes. Packing reduces page-scan bytes
by about 4.8%; batching reduces submissions by about 76%. Every admitted prefetch page
is subsequently used in the sequential traces. Accidental sequential pairs in the random
trace cause only 30 and 11 extra pages to be read. The random control therefore avoids
the large read amplification seen in the first policy.

Open time also falls in this fixture: across the four workloads, median open is about
190–192 ms before and 89 ms after. Foreground prefetch runs after open, so it cannot
explain that difference. Open still validates snapshots individually; this observation
combines changed snapshot order and page count, without isolating their contributions.
No checkpoint-write speedup is claimed from this read experiment.

### Why require sequential demand before prefetch?

The first 48-case trial prefetched on every eligible miss. It improved page-scan
throughput, but random throughput fell **22% at 4 MiB and 27% at 512 KiB**. At 4 MiB,
metadata reads grew from 21,852 to 61,105 pages, and only 2,932 of 39,340 admitted
prefetch pages were used. That failed the predeclared random-read regression trigger.

The final policy starts prefetch only when a read advances to the next mapping page
ID after the preceding read. It reads at most four pages whose current index pointers
are physically consecutive. Admission uses unused slots or clean, unreferenced CLOCK
entries, with a bounded 32-position probe. Speculation does not age hot pages, replace
resident or dirty pages, or initiate a pressure checkpoint. First use marks the page
referenced. Writes and recovery do not prefetch.

The 336-entry layout, sorted snapshot publication, and bounded read policy are measured
together. This experiment does not separately quantify the packing and sorting gains.

### Evidence and reproduction

Both sweeps passed all 48 expected cases and full-payload verification. The five engine
prefetch tests cover independent I/O accounting in cold and full caches, sorted snapshots
across write-batch boundaries, resident/dirty protection, pointer gaps, speculative
corruption, short reads, and nonsequential demand. Cache tests verify cold admission,
use/eviction counters, sorted iteration, and zero allocations. The 336-entry codec tests
verify all entries, exact offsets, checksums, and geometry boundaries.
The [release validation record](PLAN-PAGED-MAPPING.md#packing-and-prefetch-validation-2026-09-19)
links all 311 bounded/exhaustive commands and the passing live device/filesystem/SQLite gates.

- [Final sequential-trigger results](evidence/mapping-prefetch-336-sequential-2026-09-19.jsonl): SHA-256 `e4337c8ff26ded9f1925299d87f995f7143b3f0ce3f8803c85e3aece507bddcc`; candidate source SHA-256 `17f595f9fa41a74b00f3af51e2aaf3d1c46ec9347463db9508329eb5601523ff`.
- [Initial unconditional-prefetch trial](evidence/mapping-prefetch-336-2026-09-19.jsonl): SHA-256 `f3d00905f8e4e42a0390d5323c7b172e39c3b0cfb55638142798bb1710925a0d`; source SHA-256 `5608f0833aefd81f901c9bb97f3a73a3e8c1ab956a5a362186acb29550e232f7`. This is not the released policy. The [rejected-policy patch](evidence/mapping-prefetch-unconditional-2026-09-19.patch) reconstructs it from the final sources.

Raw `working` rows in the final sweep identify v1.2.3. Source digests hash sorted relative
paths and contents of the copied Rust tree, including the common driver and adapter;
metadata also retains driver/binary hashes and the pre-commit base `ec052e4`.
The implementation commits are `1dfb4ba` (packing), `39f2d59` (sorting), and `c4466e7`
(prefetch). All 85 Rust/Cargo files match the measured candidate. Benchmark tools are `a6e8821`.
The runner copies current sources and archives v1.2.2 into private build trees. To repeat
v1.2.3 exactly, run it from that tag's checkout with fresh build/output paths:

```sh
uv run --quiet python tools/mapping-bench/prefetch.py \
  --build-dir /tmp/msfblkdev-prefetch-next \
  --output evidence/mapping-prefetch-next.jsonl
uv run --quiet python tools/mapping-bench/summarize.py evidence/mapping-prefetch-next.jsonl
```

The next useful measurements are longer latency controls and fragmented or mixed access
patterns. These are engine read results, not fio or database throughput measurements.

## v1.2.2: checkpoint batching (2026-09-19)

Batching checkpoint writes fixes the measured submission regression. Against v1.2.1,
the automatic cache gains **46% fio scattered-write throughput** and **52% engine
throughput**. Full-cache final close falls from **58.06 ms to 11.66 ms**. The checkpoint
writes the same bytes with the same three fsync barriers.

This checkpoint-batching experiment compares fresh v1.1 and v1.2.1 baselines against
v1.2.2. The v1.2.3 experiment is recorded separately at the start of this report. Historical sections retain
the packed-data correction and original v1.2 measurements as separate experiments.

| Release | Behavior | Label in this experiment's raw evidence |
| --- | --- | --- |
| v1.1 | Dense mapping, 128 KiB checkpoint writes | `v1.1` |
| v1.2 | Original paged mapping, one data record per mapping page | Not rerun here |
| v1.2.1 | Packed data records, individual 4 KiB checkpoint writes | `v1.2.1` |
| v1.2.2 | Packed data records, checkpoint writes up to 128 KiB | `working` |

The candidate was measured before committing. Its raw `working` label means v1.2.2
in this experiment; the earlier packed-write experiment used `working` for v1.2.1.
The v1.2 and v1.2.1 tags remain unchanged. All paged releases use disk format 3.

### What changed in checkpoint publication?

Pressure already checkpointed **all dirty pages**. v1.2.2 keeps that policy and groups
up to 32 consecutive snapshot blocks into one positional io_uring write. It batches
index-body writes the same way, using fixed 128 KiB aligned scratch arrays. Each write
must complete exactly before the next batch. Each MappingPage retains its LSN and
checksums. Dirty pages become clean only after the descriptor fsync succeeds.

This restores v1.1's 128 KiB submission bound without adding dependencies or changing
record packing, cache allocation, checkpoint frequency, or the disk layout. We did
not sweep 2/8/16/32-page batch sizes; 32 is a measured improvement, not a proven optimum.
Partial or failed writes latch the volume and preserve dirty protection. A batch is
not crash-atomic; torn snapshot framing can still make reopening fail closed.

### Does it improve scattered writes and close?

Medians of three interleaved repetitions. These bounded workloads use a 4 GiB logical
volume, fresh 1 GiB backing, and a 4 MiB write buffer. The scattered-write trace has
4,096 operations with a flush every 32. Main throughput excludes final close.

| Engine scattered-write metric | v1.1 | v1.2.1 automatic | v1.2.2 automatic | v1.2.1 full | v1.2.2 full |
| --- | ---: | ---: | ---: | ---: | ---: |
| Throughput, kIOPS | 60.30 | 19.67 | 29.82 | 52.97 | 51.45 |
| Flush p99, ms | 0.69 | 28.62 | 6.49 | 0.79 | 0.94 |
| Data records | 128 | 128 | 128 | 128 | 128 |
| Pressure checkpoints | 0 | 3 | 3 | 0 | 0 |
| Final close, ms | 13.07 | 12.04 | 3.76 | 58.06 | 11.66 |
| Throughput including close, kIOPS | 49.99 | 18.60 | 28.90 | 30.68 | 44.43 |

At the automatic cache size, engine throughput rises **51.6%** and flush p99 falls
**77.3%**. At an explicit 4 MiB budget, throughput rises from 20.53k to 30.33k IOPS
(**47.7%**), and flush p99 falls from 27.98 ms to 6.62 ms. Both budgets still trigger
three pressure checkpoints; the improvement comes from cheaper publication.

The full-cache trace has no pressure checkpoint inside its timed writes. Its throughput
ranges overlap: 50.52–54.49k IOPS for v1.2.1 and 48.14–53.14k for v1.2.2. The gain
appears at final close: **79.9% less time**, near v1.1's close cost. Throughput including
close rises **44.8%**, reaching **88.9% of v1.1**. This does not restore every engine KPI.

| fio metric | v1.1 | v1.2.1 automatic | v1.2.2 automatic | v1.2.1 full | v1.2.2 full |
| --- | ---: | ---: | ---: | ---: | ---: |
| Scattered writes, kIOPS | 31.27 | 13.79 | 20.18 | 29.47 | 30.34 |
| Scattered-write flush p99, ms | 1.12 | 45.88 | 9.50 | 2.07 | 1.00 |
| Scattered reads, kIOPS | 16.30 | 9.80 | 9.94 | 16.55 | 16.54 |
| Scattered-read completion p99, µs | 92.67 | 181.25 | 181.25 | 89.60 | 92.67 |
| Sequential reads, kIOPS | 33.99 | 34.06 | 34.13 | 34.49 | 34.13 |
| Sequential writes, kIOPS | 81.51 | 81.51 | 83.59 | 63.02 | 85.78 |
| Scattered-read daemon RSS, MiB | 20.17 | 11.98 | 11.97 | 20.26 | 20.31 |

Automatic-cache fio scattered-write throughput improves **46.3%**, and flush p99 falls
**79.3%**. The throughput ranges are 12.76–18.12k IOPS before and 18.70–22.63k after.
Full-cache write ranges overlap: 26.95–29.90k before and 28.64–31.03k after, versus
28.25–32.25k for v1.1. There is no pressure checkpoint in that timed full-cache trace,
so its flush-tail change is not evidence of a checkpoint-batching gain. Sequential
writes also vary widely: v1.2.1 full-cache runs span 61.36–104.36k IOPS.

### Which cache size approaches v1.1?

Cache allocations are unchanged from v1.2.1. For this volume and buffer, the automatic
cache has 1,024 slots and allocates **3,977,360 bytes (3.79 MiB)**. An explicit 4 MiB
budget gives 1,080 slots and 4,192,856 allocated bytes. A 16 MiB request caps at all
3,277 mapping pages, allocating **12,684,056 bytes (12.10 MiB)**, plus a 13,108-byte
resident index. v1.1's dense mapping allocates 12 MiB. These are not process RSS limits;
the new scratch storage also sits outside the cache budget.

The full cache reaches **101% of v1.1 fio scattered-read throughput and 97% of its
scattered-write throughput** in this run. Read p99 is equal and flush p99 is lower.
That meets the provisional fio review thresholds, but three short runs do not establish
statistical equivalence. Engine throughput including close still falls below the 90%
floor, and engine flush p99 exceeds the 10% increase ceiling. No tested setting matches
every KPI.

The automatic cache still reaches only 61% of baseline fio scattered-read throughput
and 65% of scattered-write throughput. Its direct-engine scattered-read hit rate remains
31.65%, versus 100% for the full cache. Engine scattered reads are essentially unchanged:
19.02k IOPS full and 11.02k automatic, versus 19.10k for v1.1. Batching addresses write
submission cost; it does not remove metadata reads caused by cache misses.

### Do exact I/O counts explain the gain?

A separate accounting diagnostic repeats the scattered-write trace. It excludes format
and open, and reports final close separately. Its timings are not performance results.
All 25 cases passed. Between v1.2.1 and v1.2.2, every matched case retains the same
payload/record counts, backing reads, bytes written, and fsync counts.

| Backing I/O, 4,096 scattered writes, flush every 32 | v1.1 | v1.2.1 automatic | v1.2.2 automatic | v1.2.1 full | v1.2.2 full |
| --- | ---: | ---: | ---: | ---: | ---: |
| Writes before close | 128 | 3,195 | 230 | 128 | 128 |
| Bytes written before close | 17,235,968 | 29,798,400 | 29,798,400 | 17,235,968 | 17,235,968 |
| fsyncs before close | 128 | 137 | 137 | 128 | 128 |
| Final-close writes | 97 | 398 | 15 | 2,363 | 76 |
| Final-close bytes | 12,587,008 | 1,630,208 | 1,630,208 | 9,678,848 | 9,678,848 |
| Final-close fsyncs | 3 | 3 | 3 | 3 | 3 |

Full-cache close writes 2,358 mapping snapshots, four index blocks, and one descriptor.
Batching reduces **2,363 serial write completions to 76**: 74 snapshot writes, one index
write, and one descriptor write. Default-cache pressure publications likewise reduce
the writes inside the workload. This directly confirms the submission regression and
its correction without changing durability barriers or reducing persisted state.

### How were v1.2.2 results verified?

The sweep passed **105 engine cases, 60 fio cases, and 25 accounting cases**. All 150
archived fio setup/warmup/measured/verification jobs reported zero errors. Maximum
backing allocation was 93,933,568 bytes (89.58 MiB). No workload depends on reclamation.
Cleanup left no benchmark backing, live test device, or mount.

The host and workload conditions match the earlier experiment documented below:
Ryzen AI 9 HX 370, Linux 7.0.0-31-generic, Rust 1.97.1, fio 3.41; engine/daemon on CPU 0,
fio on CPU 1; direct I/O, queue depth 1, Btrfs private backings with `chattr +C`.
Correctness gates ran separately. Host frequency/background activity remain uncontrolled.
RSS is sampled, not peak. There are only 128 scattered flush samples per run.

The common workload driver is unchanged. Both measured candidate builds have source
SHA-256 `02ed4127d0af05144b4c896b790c1b96c8a58641cdaacce712e498d333eb8fd2`.
This hashes sorted relative paths and contents of the copied Rust tree, including the
driver/adapter. All 82 Rust source and Cargo files match the measured candidate.
The checkpoint implementation is commit `ff437d7`; benchmark tools are `4e43479`.
Gate filenames and raw metadata identify the pre-commit v1.2.1 base, `4c8037a`.
The [checkpoint validation record](PLAN-PAGED-MAPPING.md#checkpoint-batching-validation-2026-09-19)
links the 305-command gates, crash tests, live filesystems, and 87-case SQLite matrix.

- [Engine results](evidence/mapping-bench-checkpoint-2026-09-19.jsonl): SHA-256 `9f2412894fac59d936c02924e3047bd75fa2d2badc0a65973a8ad95bd6f638f6`.
- [fio results](evidence/mapping-ublk-checkpoint-2026-09-19/results.jsonl) and [build metadata](evidence/mapping-ublk-checkpoint-2026-09-19/metadata.json): results SHA-256 `e02b82751aebaa013d68ceec14bbfa0dcaad9c91bcff54bbe43bbf5956207dd5`.
- [Raw fio/daemon logs](evidence/mapping-ublk-checkpoint-2026-09-19/raw-logs.tar.gz): SHA-256 `2fbd284db8ed12a72cf05820576f11ca3ca312513de3b5594fc74a5008cae2b6`.
- [Exact I/O counts](evidence/mapping-io-counts-checkpoint-2026-09-19.jsonl): SHA-256 `0213fc159e3a49d871721236a05ce6c0fe0779ed6a7d13c6d554d87e2b0521d7`.

Reproduce from the release tags using fresh build/output paths. Run each command in
sequence; the fio command creates the existing operator-authorized disposable devices.

```sh
uv run --quiet python tools/mapping-bench/run.py --versions v1.1 v1.2.1 v1.2.2 \
  --build-dir /tmp/msfblkdev-v122-perf --budgets default 4194304 16777216 \
  --repetitions 3 --output evidence/mapping-bench-v122-next.jsonl
uv run --quiet python tools/mapping-bench/ublk.py --versions v1.1 v1.2.1 v1.2.2 \
  --build-dir /tmp/msfblkdev-v122-perf --output evidence/mapping-ublk-v122-next
uv run --quiet python tools/mapping-bench/io_counts.py --versions v1.1 v1.2.1 v1.2.2 \
  --build-dir /tmp/msfblkdev-v122-perf --output evidence/mapping-io-v122-next.jsonl
```

Remaining performance work includes startup validation, cache-miss cost, and longer
controlled tail-latency measurements. Inline/indirect checkpoint indexes remain future
format work in [ADR-07](ADR-07-PAGED-MAPPING-AND-CACHE.md#future-work), linked to ADR-04.

## Earlier experiment: v1.2.1 packed-data correction (2026-09-19)

Measured 2026-09-19. Sorting the whole write buffer and packing across mapping-page
boundaries restores the scattered-write record count from 4,080 to 128. With a full
mapping cache, fio write throughput reaches 34.1k IOPS, versus 33.6k on v1.1 and
14.8k on v1.2 in this rerun. Flush tails and final checkpoint cost still exceed v1.1.

These are fresh measurements of v1.1, original v1.2, and the corrected v1.2.1 implementation.
This earlier experiment compares v1.1 with v1.2.1; original v1.2 remains a diagnostic baseline. The
[historical measurements](#historical-measurements-before-the-batching-fix) retain the
earlier experiment and its raw evidence. The v1.2 tag retains the original implementation.

| Label used in this report | Implementation | Raw evidence label |
| --- | --- | --- |
| v1.1 | Original dense mapping | `v1.1` |
| v1.2 original | Unfixed release: one data record per mapping page | `v1.2` |
| v1.2.1 | Corrected release: sorted buffer, packed records across mapping pages | `working` |

Every mention of v1.2 refers to the unfixed release. The tables compare it with v1.2.1;
no fixed result is labelled v1.2. The fixed code was benchmarked before committing, so
its raw evidence retains the label `working`. Baseline numbers differ from the earlier report because
both unchanged tags were measured again in this experiment.

### What changed?

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

### What reaches the block-device frontend?

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
repetitions do not establish statistical equivalence. The observed flush-p99 difference
also needs a longer, targeted experiment before assigning a cause.

The automatic-cache fio comparison improves from v1.2's 7.21k to the fixed implementation's 16.13k scattered-write
IOPS, but flush p99 increases from 12.65 to 27.66 ms. This setting saves memory and
still falls well short of baseline performance for a widely scattered working set.

### What does the engine show at equal cache budgets?

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

### Why can a smaller checkpoint still take longer?

The index is smaller; checkpoint publication also writes every dirty mapping page.
The full-cache scattered-write sequence touches 2,358 distinct mapping pages. The
archived fio daemon report confirms `mapping_dirty_slots=2358` before close, with no
earlier pressure checkpoint. Those snapshots occupy 9,658,368 bytes (9.21 MiB).
The 13,108-byte index occupies four disk blocks, followed by one descriptor block.

The v1.2 implementation also lost checkpoint write batching. In v1.1,
`persist_checkpoint_body` streams the 12 MiB dense mapping in 128 KiB chunks.
In v1.2.1, `append_snapshots` issued one
4 KiB write per dirty page and waits for completion before the next. The index body
also uses individual 4 KiB writes. Both versions retain three fsync barriers per
checkpoint; the regression is not an increase in that barrier count.

For this particular close, the source and observed dirty-page count imply:

| Checkpoint publication | v1.1 | v1.2.1 |
| --- | ---: | ---: |
| Mapping/index/descriptor bytes | 12,587,008 | 9,678,848 |
| Backing write submissions, each waited on separately | 97 | 2,363 |
| fsync barriers | 3 | 3 |
| Measured engine close median, ms | 12.04 | 55.76 |

The submission counts are derived from the code and fixture geometry, not from a new
close-accounting run. They expose a concrete batching regression: fewer bytes but
about 24 times as many serial write completions. Their exact contribution to elapsed
time still needs profiling. Pressure checkpoints use the same publication path and
persist all current dirty pages to free cache capacity, so they incur the same pattern.

This diagnosis led to the v1.2.2 checkpoint-batching experiment at the start of this report.
Its accounting run confirms these submission counts and measures the correction.

### How were the runs bounded and verified?

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

### Reproduce and retain the evidence

The original run preserved both release tags and copied the candidate Rust sources into a private
build tree for `--working-tree`. To reproduce v1.2.1 now, select its tag explicitly. The 1 MiB candidate setting is omitted because it is
incompatible with this driver's 4 MiB buffer. Use a new output path for each run.
The ublk command uses the operator-authorized disposable-device fixtures.

```sh
uv run --quiet python tools/mapping-bench/run.py --versions v1.1 v1.2 v1.2.1 \
  --build-dir /tmp/msfblkdev-packed-perf --budgets default 4194304 16777216 \
  --repetitions 3 --output evidence/mapping-bench-next.jsonl
uv run --quiet python tools/mapping-bench/summarize.py evidence/mapping-bench-next.jsonl
uv run --quiet python tools/mapping-bench/ublk.py --versions v1.1 v1.2 v1.2.1 \
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

Checkpoint/close cost was the next performance target after this experiment; v1.2.2
results appear at the start of this report. The filesystem and SQLite runs remain correctness gates; this report
does not claim database transaction throughput or sustained overwrite performance.

## Historical measurements before the batching fix

These measurements describe original v1.2, before v1.2.1. They are a separate
experiment from the main tables and must not be substituted into that comparison.
The original sweep ran 90 engine cases, a 15-case sequential-write control,
36 fio cases, and 15 backing-I/O diagnostics. Raw results remain unchanged.

The original full-cache fio comparison reached 94.7% of v1.1 scattered-read throughput
with 23.8% higher read p99, and 64.8% of scattered-write throughput. The unchanged v1.2
read-p99 regression did not recur in the later experiment. This is run-to-run variation,
not evidence that the write-batching correction fixed a read-path regression.

The original cache sweep included intermediate budgets that the later experiment did
not repeat for v1.2.1. At 12 MiB requested, original v1.2 held 3,250 of 3,277 pages:
99.25% scattered-read hit rate, 18.47k engine IOPS, and 103.71 µs read p99. A full cache
held all pages, reached 18.57k IOPS, and had 81.32 µs p99. The corresponding v1.1
baseline was 19.1k IOPS and 80.35 µs. This shows why throughput and tails need separate
checks; it does not establish the smallest v1.2.1 cache with equivalent performance.

### Why original v1.2 emitted more backing writes

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

### Historical evidence and reproduction

The common driver and release tags are the same as in the v1.2.1 packed-data experiment. The original
report's complete prose and tables remain in Git at v1.2.1 under
`RESULTS-PERF-PAGED-MAPPING.md`; this consolidated report replaces both report paths.

- [Original engine sweep](evidence/mapping-bench-v11-v12-2026-09-19.jsonl): 90 cases; SHA-256 `2c604b061e200f74056876689ed874c4b8a265f18a2348886293338fe12801f3`.
- [Sequential-write control](evidence/mapping-bench-seq-write-control-2026-09-19.jsonl): 15 cases; SHA-256 `123576ab4ea56fe44459e8fb26ed62cc1aae1ffca37200d7bafbe18cc933ce24`.
- [Original fio results](evidence/mapping-ublk-2026-09-19/results.jsonl), [build metadata](evidence/mapping-ublk-2026-09-19/metadata.json), and [raw logs](evidence/mapping-ublk-2026-09-19/raw-logs.tar.gz): 36 cases; results SHA-256 `c33991706a8be398f9a8f790950521084adbd8fc9fe006510e209c0a74be31e4`.
- [Backing-I/O diagnostic](evidence/mapping-io-counts-2026-09-19.jsonl) and [corrupted-payload verification control](evidence/mapping-fio-verification-control-2026-09-19.json): diagnostic SHA-256 `7a6a865c5aa756100a076b5d7e21a215fd4043e787f57b872d227d1edfb75211`.

Omit `--working-tree` to repeat only the original release comparisons. Run the
accounting diagnostic separately from timed measurements:

```sh
uv run --quiet python tools/mapping-bench/io_counts.py \
  --build-dir /tmp/msfblkdev-packed-perf --output evidence/mapping-io-counts-next.jsonl
```
