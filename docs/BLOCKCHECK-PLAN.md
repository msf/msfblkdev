# Blockcheck: raw-device space and crash suite

Date: 2026-09-23
Status: plan; nothing implemented. Gates and thresholds are proposals until frozen.
Related: [ADR-10 workbench tests and reports](../ADR-10-WORKBENCH-TESTS-AND-REPORTS.md),
[ADR-08](../ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md),
[ADR-05 fault testing](../ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md)

## Why this suite?

Format 3 never reuses space, and no current test shows it. Every benchmark fits inside the finite
log. None measures how fast the backing fills, how much backing a workload needs, or what
reclamation costs.

Blockcheck answers three questions about an ADR-08 implementation:

1. Does reclamation recover space?
2. What does it cost in throughput, latency and write amplification?
3. How much does extra backing space reduce that cost?

It uses only the daemon CLI and the ublk device. It does not read the implementation.

## How does it stay impartial?

- The spec baseline is ADR-08 and the ADR-01/03 device contract at `57d56cc`.
- Each case cites the ADR-08 clause it checks. A candidate's amendments to ADR-08 are claims to
  review, not the baseline.
- Only stated invariants and contracts can fail a candidate. Open decisions (segment size,
  watermarks, victim count, idle policy) are measured, not gated.
- Gates and thresholds are frozen before the first candidate run.
- Seeds are drawn when a run starts and recorded in the manifest.
- Each version's adapter uses only that version's `--help` and README.
- The candidate's own statistics are diagnostics. A mismatch with our measurements is a finding.

## Test stack

| Layer | What we observe |
|---|---|
| Backing file on `/mnt/blockcheck` (XFS, reflink, LV `ubuntu-vg/blockcheck`) | Exact size; `st_blocks` and `SEEK_DATA` for occupied space |
| `/sys/block/dm-N/stat` of that LV | Sectors written, discards, flushes |
| ublk device | Client latency, errors, content |
| `systemd-run --user`, `taskset` | Memory cap, `memory.peak`, CPU time, pinning |
| `cp --reflink` | Copies of crash images |

Timed runs start from a fully written backing file, so XFS allocates nothing during O_DIRECT
overwrites. Space runs start from a sparse file, so `st_blocks` shows growth and hole punching.
The raw LV `blk-128mib-0` is the control for XFS overhead.

No step needs root after the one-time setup below.

### One-time setup (done 2026-09-23)

1. Create the LV: `sudo lvcreate -L 16G -n blockcheck ubuntu-vg`.
2. Format it: `sudo mkfs.xfs -L blockcheck -m reflink=1 /dev/ubuntu-vg/blockcheck`.
3. Add `/dev/ubuntu-vg/blockcheck /mnt/blockcheck xfs noatime,lazytime,nofail 0 0` to `/etc/fstab`.
4. Mount it: `sudo mount /mnt/blockcheck`.
5. Give it to the user: `sudo chown "$USER": /mnt/blockcheck`.

Before it writes, the harness checks that the backing file is on this filesystem.

## Pieces

| Piece | Location | Reuse |
|---|---|---|
| `blockcheck` crate: checker, workload generator, targets, sampler | `tools/blockcheck/`, Rust, std plus `libc` | No engine dependency, so it tests any version |
| Adapters | `tools/blockcheck/adapters/<version>` | Format, serve, find device, stop |
| Evidence | `evidence/blockcheck-<run-id>/` | ADR-10 layout |
| Report | `blockcheck` parser in `tools/workbench/evidence.py` | ADR-10 CLI, outputs, exit codes, N/A rule |

Targets implement `write`, `read`, `flush` and `discard`:

- **Device:** a ublk device or any O_DIRECT path.
- **Memory:** a simulated disk with a volatile write cache and optional faults. It tests the checker.
- **Engine (fault testing):** the engine in process over a simulated disk.
- **Cluster (replicated log):** a client of the replicated device.

## Checker rules

Each 4 KiB payload derives from (seed, LBA, version). Word 0 holds the version masked by a value
from the seed and LBA. The other words come from a keyed SplitMix64 stream. A block from another
LBA or run does not decode. This is not cryptographic.

For each LBA the checker records the latest acknowledged version, the latest durable version and
the latest discard. A successful FLUSH makes every earlier acknowledged write and discard durable.

- Without a crash, a read returns the latest version. It returns zeros only if the LBA was never
  written or its latest operation was a discard.
- After a crash, a read returns a version from the durable one to the latest one. It returns
  zeros only if nothing was durable or a discard is in that range.
- A read never returns another LBA's block, an older version, a version never acknowledged, or
  garbage.

After a crash the harness reads every LBA, then repairs each mismatch by repeating the latest
operation with a new version, and flushes. The model is exact again. These rules are per LBA;
they do not check prefix order across LBAs.

## Cases

| Case | Procedure | ADR-08 clause |
|---|---|---|
| `aged-overwrite` | Write every LBA, flush, then overwrite to a target multiple of the backing. Settings: pattern (uniform, hot/cold, moving hot region, sequential), working set (1 LBA to all), flush interval. | Cleaning cost; two heads; relocation source check; metadata reclamation |
| `write-pressure` | Write at maximum rate near the space frontier. Then write in bursts with random idle gaps. On ENOSPC, retry with backoff. | Reserve deferral; no ENOSPC with a positive-gain victim; idle cleaning |
| `discard-cycle` | Write a window, flush, discard it, flush, move to the next window. At least 10 cycles and 5 × backing. | Client discard; zeros after discard; discard ordering |
| `crash-restart` | SIGKILL the daemon at random points during the other cases and during reopen. Verify every LBA. | Device contract; invariants 1–3 |
| `harness-check` | v1.2.3 must fail `aged-overwrite` (ENOSPC) and `discard-cycle`. The memory target with each fault must fail the checker. | None; it tests the harness |

Each case reports:

- Endurance: client bytes before the first ENOSPC, divided by backing bytes.
- Write amplification: device bytes written divided by client bytes written.
- Throughput and p50/p99/p99.9/max latency, per second.
- ENOSPC count and the longest stall.
- Checker violations.
- Peak memory and CPU seconds per GiB written.

Comparisons: v1.2.3 fresh against the candidate fresh gives the format cost. The candidate fresh
against the candidate aged gives the reclamation cost. The fill-factor sweep gives the value of
extra space. Steady state means two consecutive equal windows agree within 5% on throughput and
write amplification.

## Backing sizes

Size = 8 KiB + n × 32 MiB. ADR-08 puts two descriptors in blocks 0 and 1, then whole segments.
8, 16 and 32 MiB segments all divide the rest. The floor is ADR-08's proposed
`ceil(live × 1.01 / S) + 2 + 1` segments. The endurance gate applies at 1.25 × the floor or more.
Below the floor, ENOSPC is allowed but must be clean.

| Case | Logical | Backing file | Bytes | Fill | At or above floor for S (MiB) | Gate for S (MiB) |
|---|---|---|---:|---:|---|---|
| Exhaustion | 128 MiB | 128 MiB + 8 KiB | 134,225,920 | 1.00 | none | none |
| Sweep | 128 MiB | 160 MiB + 8 KiB | 167,780,352 | 0.80 | 8 | none |
| Sweep | 128 MiB | 192 MiB + 8 KiB | 201,334,784 | 0.67 | 8, 16 | none |
| Sweep | 128 MiB | 224 MiB + 8 KiB | 234,889,216 | 0.57 | 8, 16 | 8 |
| Sweep | 128 MiB | 256 MiB + 8 KiB | 268,443,648 | 0.50 | 8, 16, 32 | 8, 16 |
| Sweep | 128 MiB | 320 MiB + 8 KiB | 335,552,512 | 0.40 | 8, 16, 32 | 8, 16, 32 |
| Fresh comparison | 128 MiB | 1 GiB + 8 KiB | 1,073,750,016 | n/a | fits v1.2.3's log | n/a |
| Large sweep | 4 GiB | 5 GiB + 8 KiB | 5,368,717,312 | 0.80 | 8, 16, 32 | none |
| Large gate | 4 GiB | 6 GiB + 8 KiB | 6,442,459,136 | 0.67 | 8, 16, 32 | 8, 16, 32 |

`discard-cycle` needs a logical size of at least twice its window, for example 1 GiB logical over
a 256 MiB + 8 KiB backing with a 128 MiB window.

## ENOSPC under pressure

ADR-08: "At the reserve, defer user writes that would consume protected space." ublk can defer a
write by holding the request.

| Outcome | Grade |
|---|---|
| Writes slow down; no ENOSPC | Pass |
| Brief ENOSPC; a retry (backoff at most 1 s) succeeds within 30 s; no flushed data lost | Pass, deviation reported |
| ENOSPC past 30 s while space is reclaimable; EIO; failed volume; lost data | Fail |

Unverified: ext4 and XFS probably treat a failed write as an I/O error. If so, brief ENOSPC is
acceptable only for raw-device clients.

## Proposed gates

| Gate | Pass condition |
|---|---|
| Correctness | Zero checker violations in every run. |
| Endurance | `aged-overwrite` writes at least 20 × backing (128 MiB scale) or 3 × backing (4 GiB scale) without ENOSPC, on every gated size. |
| Discard | `discard-cycle` completes without ENOSPC; discarded LBAs read zeros after FLUSH and restart. |
| Exhaustion | Real exhaustion returns ENOSPC, not EIO or a hang; flushed data survives; writes resume after discard. |
| Memory | No OOM under the frozen cap; peak memory has no upward trend over 10 cycles. |
| Stalls | No request exceeds the frozen deadline under `write-pressure`, excluding ENOSPC retries. |

## Open decisions (freeze before the first candidate run)

- Regression threshold for the fresh comparison. The template says throughput ≥90% and P99 ≤110%;
  the implementation prompt says >10% throughput loss or >20% P99 increase.
- Memory cap per scale.
- Stall deadline (proposal: 1 s) and ENOSPC retry limits (proposal: 1 s backoff, 30 s total).
- Endurance write multiples (proposal: 20× and 3×).

## Evidence

```text
evidence/blockcheck-<run-id>/
  manifest.json          versions, SHAs, binary hashes, seed, kernel, CPU, backing, frozen gates
  <case>/case.json       settings, results, gate outcomes
  <case>/timeline.csv    per second: ops, latency percentiles, device counters, st_blocks, memory
  <case>/daemon.log      daemon stderr
  summary.json
```

## Deliveries

1. Checker, payload, memory target with faults, generator and device target. Unit tests prove
   each fault fails the checker.
2. v1.2.3 adapter and sampler. Run `aged-overwrite` and `harness-check` on v1.2.3. Calibrate
   against `fio` and measure XFS overhead against the raw LV.
3. The remaining cases, the sweep and the ADR-10 parser. Freeze the gates. Run blind against
   `v2.0-rc`.
4. Fault testing (ADR-05): the same checker over a simulated disk, first in process, later as an
   unprivileged ublk backing target.
