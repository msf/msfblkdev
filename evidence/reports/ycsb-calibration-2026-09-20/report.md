# Workbench evidence report

Descriptive comparison of selected evidence. Correctness and calibration are separate result classes.

N/A means unavailable, never zero. CSV blanks and JSON nulls have the same meaning.
Backing I/O covers the workload plus drain, excludes initialization and validation, and is not occupied physical space.
YCSB operation rates use logical requests; F's API-call latency counts include its additional updates.
The CSV retains each repetition, load/drain times, request/flush counts and peak RSS where measured.
The JSON retains source paths, configuration, all API latency summaries and failures. No raw evidence is changed.

| Column | Run | FS | Kind | Status | Selected samples |
| --- | --- | --- | --- | --- | --- |
| Run 1 | raw-ext4-timing-1789929219527797136 | ext4-host | ycsb-host-calibration | incomplete | 2 |
| Run 2 | raw-ext4-timing-1789929366350639320 | ext4-host | ycsb-host-calibration | calibration | 2 |
| Run 3 | raw-ext4-timing-1789930051819327082 | ext4-host | ycsb-host-calibration | calibration | 2 |

## Workload seconds by case

Columns follow the input-run order above. Medians only; configuration differences remain visible in the per-run sections and JSON.
N/A can also mean that this case was not selected in that input. No percentage-change or regression verdict is inferred.

| DB / case / profile | Run 1 | Run 2 | Run 3 |
| --- | --- | --- | --- |
| rocksdb / a / sync-writes | N/A | 32.132 | N/A |
| rocksdb / e75 / sync-writes | N/A | N/A | 22.706 |
| rocksdb / f / sync-writes | N/A | 32.678 | N/A |
| sqlite / a / sync-writes | 18.904 | N/A | N/A |
| sqlite / e75 / sync-writes | N/A | N/A | 19.703 |
| sqlite / f / sync-writes | 19.850 | N/A | N/A |

## raw-ext4-timing-1789929219527797136

ycsb-host-calibration: **incomplete**. Source: `/home/miguel/play/msfblkdev/evidence/raw-ext4-timing-1789929219527797136`.

- Cached host ext4, 512-byte sectors; not a VM or storage comparison. Throughput uses process elapsed time, including open/close. Device I/O and occupied space unavailable.
- Fixture cleanup is not confirmed.

Values are per-case medians across the listed repetitions; single samples remain single samples.

| DB | FS | Case | Profile | Samples OK | Workload s | Ops/s | Backing read MiB | Backing written MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| sqlite | ext4-host | a | sync-writes | 1/1 | 18.904 | 6,933.484 | N/A | N/A |
| sqlite | ext4-host | f | sync-writes | 1/1 | 19.850 | 6,603.087 | N/A | N/A |

Final cumulative API-call latency summaries, not averaged interval percentiles. Small tail counts are weak evidence.

| DB | Case | API operation | Samples | p99 µs | p99.99 µs | Samples above p99.99 ≈ |
| --- | --- | --- | --- | --- | --- | --- |
| sqlite | a | READ | 65494 | 86 | 510 | 6.549 |
| sqlite | a | TOTAL | 131072 | 355 | 5543 | 13.107 |
| sqlite | a | UPDATE | 65578 | 449 | 6631 | 6.558 |
| sqlite | f | READ | 131072 | 78 | 501 | 13.107 |
| sqlite | f | READ_MODIFY_WRITE | 65578 | 521 | 5327 | 6.558 |
| sqlite | f | TOTAL | 196650 | 290 | 4795 | 19.665 |
| sqlite | f | UPDATE | 65578 | 430 | 5287 | 6.558 |


## raw-ext4-timing-1789929366350639320

ycsb-host-calibration: **calibration**. Source: `/home/miguel/play/msfblkdev/evidence/raw-ext4-timing-1789929366350639320`.

- Cached host ext4, 512-byte sectors; not a VM or storage comparison. Throughput uses process elapsed time, including open/close. Device I/O and occupied space unavailable.

Values are per-case medians across the listed repetitions; single samples remain single samples.

| DB | FS | Case | Profile | Samples OK | Workload s | Ops/s | Backing read MiB | Backing written MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| rocksdb | ext4-host | a | sync-writes | 1/1 | 32.132 | 4,079.210 | N/A | N/A |
| rocksdb | ext4-host | f | sync-writes | 1/1 | 32.678 | 4,011.051 | N/A | N/A |

Final cumulative API-call latency summaries, not averaged interval percentiles. Small tail counts are weak evidence.

| DB | Case | API operation | Samples | p99 µs | p99.99 µs | Samples above p99.99 ≈ |
| --- | --- | --- | --- | --- | --- | --- |
| rocksdb | a | READ | 65494 | 106 | 450 | 6.549 |
| rocksdb | a | TOTAL | 131072 | 583 | 4303 | 13.107 |
| rocksdb | a | UPDATE | 65578 | 646 | 4559 | 6.558 |
| rocksdb | f | READ | 131072 | 106 | 487 | 13.107 |
| rocksdb | f | READ_MODIFY_WRITE | 65578 | 693 | 4347 | 6.558 |
| rocksdb | f | TOTAL | 196650 | 523 | 3649 | 19.665 |
| rocksdb | f | UPDATE | 65578 | 604 | 4303 | 6.558 |


## raw-ext4-timing-1789930051819327082

ycsb-host-calibration: **calibration**. Source: `/home/miguel/play/msfblkdev/evidence/raw-ext4-timing-1789930051819327082`.

- Cached host ext4, 512-byte sectors; not a VM or storage comparison. Throughput uses process elapsed time, including open/close. Device I/O and occupied space unavailable.

Values are per-case medians across the listed repetitions; single samples remain single samples.

| DB | FS | Case | Profile | Samples OK | Workload s | Ops/s | Backing read MiB | Backing written MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| sqlite | ext4-host | e75 | sync-writes | 1/1 | 19.703 | 6,652.333 | N/A | N/A |
| rocksdb | ext4-host | e75 | sync-writes | 1/1 | 22.706 | 5,772.570 | N/A | N/A |

Final cumulative API-call latency summaries, not averaged interval percentiles. Small tail counts are weak evidence.

| DB | Case | API operation | Samples | p99 µs | p99.99 µs | Samples above p99.99 ≈ |
| --- | --- | --- | --- | --- | --- | --- |
| sqlite | e75 | INSERT | 32790 | 540 | 4093 | 3.279 |
| sqlite | e75 | SCAN | 98282 | 496 | 933 | 9.828 |
| sqlite | e75 | TOTAL | 131072 | 511 | 2781 | 13.107 |
| rocksdb | e75 | INSERT | 32790 | 610 | 6211 | 3.279 |
| rocksdb | e75 | SCAN | 98282 | 243 | 675 | 9.828 |
| rocksdb | e75 | TOTAL | 131072 | 522 | 4083 | 13.107 |

