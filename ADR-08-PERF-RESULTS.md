# ADR-08 performance results

Basic performance validation of the format-4 engine (`v2.0-rc`, candidate commit `8c7f650`)
against the last format-3 engine (`57d56cc`, baseline), as [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md)
requires: pre-cleaning behaviour, aged raw reclamation at 128 MiB and 4 GiB, progress and latency
under maintenance, and memory and restart. Both engines run the same driver
(`tools/adr08-bench/main.rs`) through a per-version adapter, built in release mode from a copy of
each tree, pinned to one CPU, with three interleaved repetitions per job. Raw records, CSV and the
generated reports are under `evidence/adr08-implementation/perf/`; the tables below come from
`report-tmpfs-8c7f650.md`, the run on the tagged candidate. Earlier runs on `2303843` (before the
final review fixes) are kept beside it and differ by a few percent.

## Method

| Item | Value |
|---|---|
| Machine | Linux 7.0.0-31-generic, 24 CPUs, 61 GiB RAM; runs pinned to CPU 0 with `taskset` |
| Toolchain | rustc 1.97.1, `--release`, `--locked` |
| Driver | 4 KiB writes through `write_block` with a 4 MiB write buffer, `flush` every 256 writes; reads through `read_block` with content verification |
| Ticks | The candidate runs the daemon's scheduler policy: `maintain_for(Idle, 2 ms)` when 100 ms passed since the last tick; the driver never idles, so only due ticks fire |
| Fixtures | Primary: tmpfs (page-cache bound, no compression). Secondary: btrfs with `compress=zstd:1` and `chattr +C`, the repository's filesystem |
| Repetitions | 3, jobs shuffled per repetition with a fixed seed; medians reported |

The tmpfs run isolates engine costs. On the btrfs fixture the numbers are dominated by the
filesystem (compression, copy-on-write, asynchronous discard) and vary between runs by more than the
engine differences; that report is kept as a data point, not as the comparison.

## 1. Fresh volume, nothing to reclaim (tmpfs)

128 MiB volume, 512 MiB backing, 16,384 operations.

| Job | Baseline ops/s | Candidate ops/s | Change | Baseline flush p99 | Candidate flush p99 |
|---|---:|---:|---:|---:|---:|
| Sequential write | 1,054,644 | 1,054,657 | +0.0% | 0.33 ms | 0.28 ms |
| Random write | 989,274 | 1,011,842 | +2.3% | 0.26 ms | 0.39 ms |
| Sequential read | 467,901 | 479,940 | +2.6% | — | — |
| Random read | 463,346 | 468,504 | +1.1% | — | — |

Read p50/p99 are 2/3 µs for both engines. Resident memory is 7.5 MiB for the baseline and
10.8–10.9 MiB for the candidate (cleaner window buffers, second view, segment state).

On the btrfs fixture the same jobs gave 147k/321k (baseline) against 113k/106k (candidate) ops/s for
sequential/random writes with 2–4× longer flushes; a controlled A/B on a copy-on-write-disabled
directory put the two engines within 8% and under `strace -c` they issued the same number of
`io_uring_enter` calls per release. Fresh-volume throughput is not a KPI of this design; the
filesystem effect is recorded here so nobody reads the btrfs table as an engine regression.

## 2. Aged raw reclamation (tmpfs)

A working set is written once, then random blocks inside it are overwritten until twice the log
capacity has been written. The baseline has no reclamation and stops at the first sixteen
`ENOSPC` results; the candidate must complete every write.

| Job | Version | Completed | ENOSPC | ops/s | op p99.9 | op max | Copies | Copies per write | Segments freed | Reopen |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 128 MiB, 50% live (64 MiB live, 256 MiB backing) | baseline | 49,005 | 16 | 1,101,248 | 1 µs | 0.00 ms | — | — | — | 0.74 ms |
| | candidate | 131,072 | 0 | 574,991 | 1 µs | 29.8 ms | 11,823 | 0.09 | 52 | 2.46 ms |
| 128 MiB, 80% live (64 MiB live, 160 MiB backing) | baseline | 24,383 | 16 | 1,041,127 | 0 µs | 0.00 ms | — | — | — | 0.92 ms |
| | candidate | 81,920 | 0 | 333,229 | 1 µs | 20.1 ms | 45,942 | 0.56 | 59 | 3.45 ms |
| 4 GiB, 50% live (1 GiB live, 2 GiB backing) | baseline | 246,903 | 16 | 722,413 | 1 µs | 0.02 ms | — | — | — | 3.86 ms |
| | candidate | 1,048,576 | 0 | 184,098 | 2 µs | 40.2 ms | 558,169 | 0.53 | 890 | 8.08 ms |

KPIs:

- **No `ENOSPC` while live data fits.** The candidate sustained twice the log capacity of
  overwrites at 50% and 80% fill on both scales with zero `ENOSPC`; the baseline failed after
  0.19–0.4 of one capacity.
- **Copy amplification** was 0.09 copies per user write at 50% fill and 0.53–0.56 at 80% fill
  (128 MiB) and at the 4 GiB 50% job. Lomet and Luo's uniform model predicts `u / (1 - u)` extra
  copy writes per user write, that is 1.0 at 50% and 4.0 at 80%; the measured values are lower
  because footers count as dead space, the cleaner picks the emptiest segments under pressure and
  idle cleaning waits for a quarter of a segment to die.
- **Bounded stalls.** The worst single write took 20–40 ms; that write ran pressure reclamation
  inline (scan wait, copies and up to two publications). p99.9 stayed at 1–2 µs because 99.9% of
  writes only fill the buffer.
- **Throughput under aging** was 0.52, 0.32 and 0.25 of the baseline's fresh-volume rate at the
  three jobs; all of the difference is reclamation work on the writer thread.
- **Restart after aging** took 2.5–8.1 ms, against 0.7–3.9 ms for the baseline's smaller volume.

## 3. Progress and latency: scheduled ticks against inline pressure only (tmpfs)

| Job | Completed | ops/s | op max | Ticks | Tick p99 | Pressure steps | Pressure waits | Copies |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 128 MiB 50%, inline pressure only | 131,072 | 576,962 | 29.8 ms | 0 | — | 976 | 451 | 11,823 |
| 128 MiB 50%, with due ticks | 131,072 | 562,206 | 31.2 ms | 2 | 1 µs | 977 | 452 | 11,823 |

Under a saturating single-threaded driver there are no idle gaps, so only the 100 ms due ticks fire
(two in a 0.23 s run), and with half the log free they rest at once (decision 23), so both rows do
the same work; the idle policy needs idle gaps, which the ublk daemon has between requests and the
driver does not. Pressure waits (a write blocked on the cleaner's window read) occurred in about
46% of pressure steps. In the live ublk-fio run on the same commit the daemon ran 615 ticks with 177
cleaner wake-ups across 26 daemon lifetimes; before the wake-up fix one lifetime saw 90,414
wake-ups for 156 ticks.

## 4. Memory and restart (tmpfs)

`heap total` is the ADR-08 budget (`max(1% of the volume, 16 MiB)` or the explicit total);
`heap allocated` is what the engine preallocated inside it. RSS includes the io_uring rings, thread
stacks and the driver.

| Job | Version | Open | RSS after open | RSS after writes | Reopen | Heap total | Heap allocated | Cache |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 128 MiB, default budget | baseline | 1.89 ms | 7.2 MiB | 7.4 MiB | 2.11 ms | — | — | 389 KiB |
| | candidate | 4.50 ms | 10.5 MiB | 10.7 MiB | 4.01 ms | 16.0 MiB | 7.3 MiB | 389 KiB |
| 1 GiB, 12 MiB total | baseline | 2.25 ms | 9.8 MiB | 10.5 MiB | 2.95 ms | — | — | 3.0 MiB |
| | candidate | 4.73 ms | 13.2 MiB | 13.8 MiB | 4.64 ms | 12.0 MiB | 10.0 MiB | 3.0 MiB |
| 1 GiB, default budget | baseline | 2.34 ms | 9.8 MiB | 10.5 MiB | 3.04 ms | — | — | 3.0 MiB |
| | candidate | 5.07 ms | 13.2 MiB | 13.8 MiB | 5.26 ms | 16.0 MiB | 10.0 MiB | 3.0 MiB |
| 4 GiB, default budget | baseline | 2.43 ms | 10.8 MiB | 11.5 MiB | 3.23 ms | — | — | 4.0 MiB |
| | candidate | 6.39 ms | 22.2 MiB | 22.8 MiB | 6.70 ms | 41.0 MiB | 19.1 MiB | 12.1 MiB |

KPIs:

- A **1 GiB volume opens and runs under a 12 MiB total** with 10.0 MiB allocated and 13.9 MiB RSS.
  The baseline was given the same total minus its write buffer as cache budget for fairness.
- The **4 GiB volume** allocates 19.1 MiB of its 41 MiB budget (the mapping cache takes 12.1 MiB)
  and stays at 22.9 MiB RSS.
- **Open** takes 4.5–6.4 ms against 1.9–2.4 ms: format 4 loads and validates the table pages and
  every referenced snapshot before replay.
- Steady-state writes, reads and maintenance allocate nothing after open
  (`steady_state_writes_reads_and_maintenance_allocate_nothing`).

## Reproduction

```sh
cd /path/to/clone && export CARGO_TARGET_DIR="$PWD/rust/target"
python3 tools/adr08-bench/run.py --output evidence/adr08-implementation/perf/adr08-bench-tmpfs-8c7f650.jsonl \
    --fixture-dir /tmp/some-tmpfs-dir --repetitions 3
python3 tools/adr08-bench/summarize.py evidence/adr08-implementation/perf/adr08-bench-tmpfs-8c7f650.jsonl \
    --csv evidence/adr08-implementation/perf/adr08-bench-tmpfs-8c7f650.csv \
    --report evidence/adr08-implementation/perf/report-tmpfs-8c7f650.md
```

`run.py` builds the baseline from `git archive 57d56cc` and the candidate from the working tree,
each in its own target directory under `rust/target/adr08-bench/`.
