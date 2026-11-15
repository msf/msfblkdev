# Performance benchmark template

Fill this in before a comparison, then append measurements to the workstream's results
file. ADR-07 uses [ADR-07-PERF-RESULTS.md](ADR-07-PERF-RESULTS.md).

## What are we testing?

- Date, hypothesis, and primary KPI: `<fill in>`.
- Baseline and candidate tags, commit SHAs, and benchmark driver SHA: `<fill in>`.
- Review thresholds: throughput at least 90% of baseline; P99 at most 110% of baseline.
  These are provisional regression triggers, not proof of statistical equivalence.
- Scope: `<engine / ublk-fio; workloads and cache settings; omitted measurements>`.

For a v1.1 parity claim, compare v1.1 directly with the candidate in the same experiment.
A comparison against the previous release answers a separate question.

## What stays fixed?

Record host/drive/filesystem, kernel, Rust version, build profile, CPU affinity,
O_DIRECT, queue depth, block size, flush frequency, and checkpoint interval.
Record logical/backing sizes, write-buffer size, trace/seed, operation count, warmup,
verification method, and whether verification is timed.

Test the candidate's automatic cache and a cache that holds the complete mapping.
Record requested/allocated cache bytes, slots, resident index bytes, and sampled or
peak RSS separately. For cache-policy comparisons, use equal byte budgets.
Keep the write buffer identical and compatible with every tested cache configuration.

Use identical logical addresses across versions, even when mapping-page capacity changes.
State mapping-cache and host-cache warmth separately. Use fresh disposable fixtures and
a fixed operation count that fits the append log, including metadata and checkpoints.
Record maximum log usage; do not rely on space reclamation.

## Which measurements do we run?

| Workload | Primary measurements |
| --- | --- |
| Sequential and seeded scattered 4 KiB reads | IOPS; read P99 |
| Sequential and seeded scattered 4 KiB writes, with fixed flush frequency | IOPS excluding and including close; flush P99 |
| Scattered writes exceeding cache capacity | Pressure-checkpoint count and duration; flush P99 |
| Clean reopen and close after the same write trace | Open and close duration; metadata bytes and submissions |

Add a targeted workload for the hypothesis, such as cold and steady mapping-page scans
for prefetch. Keep the random-read control. Record cache misses, metadata I/O, data-record
count, and prefetch usefulness where they explain the result.

Run at least three interleaved baseline/candidate repetitions without concurrent tests.
Verify payloads. Report the median and range across runs, including each run's P99 and
sample count. Three short runs are a screening test; extend noisy or borderline cases
before claiming tail-latency parity, while keeping each fixture within its log capacity.

## What did we measure?

| Workload / cache | KPI and unit | Baseline median [range] | Candidate median [range] | Change % | Within threshold / regression / inconclusive |
| --- | --- | --- | --- | --- | --- |
| `<fill in>` | `<fill in>` | | | | |

Calculate change as `100 * (candidate / baseline - 1)`; lower latency is better.
Link exact commands, raw per-run results, source/binary hashes, and correctness evidence.
Keep engine and fio results separate. Mark omitted measurements as **not run**.

State which criteria passed, failed, or remain uncertain. Name the smallest **tested**
cache that meets every declared criterion; if none does, say so. Do not infer parity
by combining percentage gains from different experiments. Record the next test needed.
