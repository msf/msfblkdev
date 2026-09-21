# ADR-08 performance results

This report compares the format-4 engine (`v2.0-rc`, candidate commit `8c7f650`) with the last
format-3 engine (`57d56cc`, baseline). [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md)
requires four comparisons:

1. Pre-cleaning behaviour on a fresh volume.
2. Aged raw reclamation at 128 MiB and 4 GiB.
3. Progress and latency under maintenance.
4. Memory and restart.

Both engines run the same driver (`tools/adr08-bench/main.rs`) through a per-version adapter. Each
version is built in release mode from a copy of its tree, pinned to one CPU, and run three times per
job with the repetitions interleaved. Raw records, CSV files and the generated reports are under
`evidence/adr08-implementation/perf/`. The tables below come from `report-tmpfs-8c7f650.md`, the
run on the tagged candidate. Earlier runs on `2303843` (before the final review fixes) are kept
beside it. They differ by a few percent.

## Method

| Item | Value |
|---|---|
| Machine | Linux 7.0.0-31-generic, 24 CPUs, 61 GiB RAM; runs pinned to CPU 0 with `taskset` |
| Toolchain | rustc 1.97.1, `--release`, `--locked` |
| Driver | 4 KiB writes through `write_block` with a 4 MiB write buffer, `flush` every 256 writes; reads through `read_block` with content verification |
| Ticks | The candidate runs the daemon's scheduler policy: `maintain_for(Idle, 2 ms)` when 100 ms passed since the last tick; the driver never idles, so only due ticks fire |
| Fixtures | Primary: tmpfs (page-cache bound, no compression). Secondary: btrfs with `compress=zstd:1` and `chattr +C`, the repository's filesystem |
| Repetitions | 3, jobs shuffled per repetition with a fixed seed; medians reported |

The tmpfs run isolates the engine costs. On the btrfs fixture the filesystem dominates the numbers
(compression, copy-on-write, asynchronous discard). Those numbers vary between runs by more than the
difference between the engines. The btrfs report is kept as a data point, not as the comparison.

## 1. Fresh volume, nothing to reclaim (tmpfs)

128 MiB volume, 512 MiB backing, 16,384 operations.

| Job | Baseline ops/s | Candidate ops/s | Change | Baseline flush p99 | Candidate flush p99 |
|---|---:|---:|---:|---:|---:|
| Sequential write | 1,054,644 | 1,054,657 | +0.0% | 0.33 ms | 0.28 ms |
| Random write | 989,274 | 1,011,842 | +2.3% | 0.26 ms | 0.39 ms |
| Sequential read | 467,901 | 479,940 | +2.6% | — | — |
| Random read | 463,346 | 468,504 | +1.1% | — | — |

Read p50 and p99 are 2 and 3 µs for both engines. Resident memory is 7.5 MiB for the baseline and
10.8 to 10.9 MiB for the candidate. The difference is the cleaner's window buffers, the second view
and the segment state.

On the btrfs fixture the same jobs gave 147k and 321k ops/s (baseline) against 113k and 106k ops/s
(candidate) for sequential and random writes, with flushes 2 to 4 times longer. Two checks show that
this is a filesystem effect and not an engine regression:

- A controlled A/B run on a directory with copy-on-write disabled put the two engines within 8%.
- Under `strace -c` both engines issued the same number of `io_uring_enter` calls per release.

Fresh-volume throughput is not a KPI of this design. The btrfs numbers are recorded here so that
nobody reads the btrfs table as an engine regression.

## 2. Aged raw reclamation (tmpfs)

The job writes a working set once. It then overwrites random blocks inside the working set until
it has written twice the log capacity. The baseline has no reclamation and stops at the sixteenth
`ENOSPC` result. The candidate must complete every write.

| Job | Version | Completed | ENOSPC | ops/s | op p99.9 | op max | Copies | Copies per write | Segments freed | Reopen |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 128 MiB, 50% live (64 MiB live, 256 MiB backing) | baseline | 49,005 | 16 | 1,101,248 | 1 µs | 0.00 ms | — | — | — | 0.74 ms |
| | candidate | 131,072 | 0 | 574,991 | 1 µs | 29.8 ms | 11,823 | 0.09 | 52 | 2.46 ms |
| 128 MiB, 80% live (64 MiB live, 160 MiB backing) | baseline | 24,383 | 16 | 1,041,127 | 0 µs | 0.00 ms | — | — | — | 0.92 ms |
| | candidate | 81,920 | 0 | 333,229 | 1 µs | 20.1 ms | 45,942 | 0.56 | 59 | 3.45 ms |
| 4 GiB, 50% live (1 GiB live, 2 GiB backing) | baseline | 246,903 | 16 | 722,413 | 1 µs | 0.02 ms | — | — | — | 3.86 ms |
| | candidate | 1,048,576 | 0 | 184,098 | 2 µs | 40.2 ms | 558,169 | 0.53 | 890 | 8.08 ms |

KPIs:

- **No `ENOSPC` while live data fits.** The candidate completed twice the log capacity of
  overwrites at 50% and 80% fill on both scales, with zero `ENOSPC`. The baseline failed after
  0.19 to 0.4 of one capacity.
- **Copy amplification.** The candidate made 0.09 copies per user write at 50% fill on 128 MiB. It
  made 0.53 to 0.56 copies per write at 80% fill on 128 MiB and at 50% fill on 4 GiB. Lomet and
  Luo's uniform model predicts `u / (1 - u)` extra copy writes per user write: 1.0 at 50% and 4.0
  at 80%. The measured values are lower for three reasons: footers count as dead space, the cleaner
  picks the emptiest segments under pressure, and idle cleaning waits until a quarter of a segment
  is dead.
- **Bounded stalls.** The worst single write took 20 to 40 ms. That write ran pressure reclamation
  inline: a scan wait, copies and up to two publications. The p99.9 stayed at 1 to 2 µs because
  99.9% of writes only fill the buffer.
- **Throughput under aging** was 0.52, 0.32 and 0.25 of the baseline's fresh-volume rate on the
  three jobs. All of the difference is reclamation work on the writer thread.
- **Restart after aging** took 2.5 to 8.1 ms. The baseline's smaller volume took 0.7 to 3.9 ms.

## 3. Progress and latency: scheduled ticks against inline pressure only (tmpfs)

| Job | Completed | ops/s | op max | Ticks | Tick p99 | Pressure steps | Pressure waits | Copies |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 128 MiB 50%, inline pressure only | 131,072 | 576,962 | 29.8 ms | 0 | — | 976 | 451 | 11,823 |
| 128 MiB 50%, with due ticks | 131,072 | 562,206 | 31.2 ms | 2 | 1 µs | 977 | 452 | 11,823 |

Both rows do the same work, for two reasons:

- The driver is a saturating single thread, so there are no idle gaps. Only the 100 ms due ticks
  fire: two in a 0.23 s run.
- Half of the log is free during the run, so the due ticks rest at once (ADR-08 decision 19).

The idle policy needs idle gaps. The ublk daemon has them between requests. The driver does not.

Pressure waits (a write blocked on the cleaner's window read) occurred in about 46% of the
pressure steps. In the live ublk-fio run on the same commit, the daemon ran 615 ticks with 177
cleaner wake-ups across 26 daemon lifetimes. Before the wake-up fix, one lifetime saw 90,414
wake-ups for 156 ticks.

## 4. Memory and restart (tmpfs)

`heap total` is the ADR-08 budget: `max(1% of the volume, 16 MiB)`, or the explicit total. `heap
allocated` is what the engine preallocated inside the budget. RSS includes the io_uring rings, the
thread stacks and the driver.

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

- **A 1 GiB volume opens and runs under a 12 MiB total.** It allocates 10.0 MiB and reaches
  13.9 MiB RSS. For fairness, the baseline received the same total minus its write buffer as its
  cache budget.
- **The 4 GiB volume** allocates 19.1 MiB of its 41 MiB budget. The mapping cache takes 12.1 MiB of
  that. RSS stays at 22.9 MiB.
- **Open** takes 4.5 to 6.4 ms, against 1.9 to 2.4 ms for the baseline. Format 4 loads and
  validates the table pages and every referenced snapshot before replay.
- **Steady state allocates nothing.** After open, writes, reads and maintenance make no heap
  allocation (`steady_state_writes_reads_and_maintenance_allocate_nothing`).

## Reproduction

```sh
cd /path/to/clone && export CARGO_TARGET_DIR="$PWD/rust/target"
python3 tools/adr08-bench/run.py --output evidence/adr08-implementation/perf/adr08-bench-tmpfs-8c7f650.jsonl \
    --fixture-dir /tmp/some-tmpfs-dir --repetitions 3
python3 tools/adr08-bench/summarize.py evidence/adr08-implementation/perf/adr08-bench-tmpfs-8c7f650.jsonl \
    --csv evidence/adr08-implementation/perf/adr08-bench-tmpfs-8c7f650.csv \
    --report evidence/adr08-implementation/perf/report-tmpfs-8c7f650.md
```

`run.py` builds the baseline from `git archive 57d56cc` and the candidate from the working tree.
Each build has its own target directory under `rust/target/adr08-bench/`.
