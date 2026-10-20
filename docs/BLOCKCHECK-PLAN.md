# Blockcheck: raw-device space and crash suite

Date: 2026-09-23
Revised: 2026-10-20
Status: planned device suite. The checker and memory target pass the fixed checker tests
(2026-10-20, commit `29a184d`).
Gate values were decided on 2026-10-20. They freeze when this file is committed, before the first
candidate run.
Related: [ADR-10 workbench tests and reports](../ADR-10-WORKBENCH-TESTS-AND-REPORTS.md),
[ADR-08](../ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md),
[ADR-05 fault testing](../ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md)

## Why this suite?

Format 3 never reuses space. Existing ENOSPC tests cover finite-log exhaustion. Blockcheck measures
how long repeated overwrites can continue, how much backing space they need and what reclamation costs.

Reclamation is implemented on a separate branch and remains under operator evaluation. Record the
exact build used for each test. This checkout still uses format 3. Blockcheck answers three questions:

1. Does reclamation recover space?
2. What does it cost in throughput, latency and write amplification?
3. How much does extra backing space reduce that cost?

The device comparison uses only the daemon CLI and the ublk device. ADR-05 separately tests
internal structures and backing faults. Those results cannot replace the device comparison.

## How does it stay impartial?

- The spec baseline is ADR-08 and the ADR-01/03 device contract at `57d56cc`.
- Each case cites the ADR-08 clause it checks. A candidate's amendments to ADR-08 are claims to
  review, not the baseline.
- Only stated invariants and contracts can fail a candidate. Open decisions (segment size,
  watermarks, victim count, idle policy) are measured, not gated.
- Gates and thresholds are frozen before the first candidate run.
- Random runs draw a seed at startup and record it in the manifest. Fixed regression sequences
  remain unchanged. Record every operation: a seed alone cannot reproduce a run if random draws change.
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

After the one-time setup below, only `make setup-blockcheck-host` needs root, once per boot.

### One-time setup (backing done 2026-09-23, ublk done 2026-10-20)

1. Create the LV: `sudo lvcreate -L 16G -n blockcheck ubuntu-vg`.
2. Format it: `sudo mkfs.xfs -L blockcheck -m reflink=1 /dev/ubuntu-vg/blockcheck`.
3. Add `/dev/ubuntu-vg/blockcheck /mnt/blockcheck xfs noatime,lazytime,nofail 0 0` to `/etc/fstab`.
4. Mount it: `sudo mount /mnt/blockcheck`.
5. Give it to the user: `sudo chown "$USER": /mnt/blockcheck`.
6. Complete the README "Normal-user ublk setup": helper, udev rule, `plugdev` membership and the
   `/run/ublksrvd` tmpfiles rule.

After each boot, run `make setup-blockcheck-host`. It loads `ublk_drv` and verifies steps 1 to 6.
It installs nothing.

Before it writes, the harness checks that the backing file is on this filesystem. It also checks
that `/dev/ublk-control` is readable and writable. If a check fails, the harness stops and prints
`make setup-blockcheck-host`. The harness never calls sudo.

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

The checker records each LBA's operation history. It distinguishes successful writes and discards,
successful flushes, rejections that left storage unchanged, and operations whose effect is uncertain.

A successful FLUSH makes earlier completed writes and discards durable. A failed flush makes no
new promise, but some pending writes may survive. An uncertain write may also reach storage before
the caller receives a result.

Before a crash, a read must return the latest successfully completed content, unless an operation's
effect is uncertain. It returns zeros only for an unwritten or discarded LBA. A read must never
return another LBA's block, garbage or data from an operation known to have been rejected without effect.

After a crash, the checker uses the operation history and the agreed device contract to decide
which values may survive. Each adapter must cite the contract's ordering and atomicity guarantees.
It cannot weaken them.

The checker must preserve the state established by the last successful flush and allow later writes
or uncertain operations only as the contract permits.

For example, the test writes three versions to one LBA:

1. Write v1 and flush successfully.
2. Reject v2 without changing storage.
3. Write v3 successfully, then crash before the next flush.

The checker must accept v1 or v3 and reject v2. Accepting every version number between v1 and v3
would miss the bug.

In this suite, `NoSpace` means the failed write left storage unchanged. A backing `ENOSPC` error
alone does not provide that guarantee. Adapters must distinguish rejection
before I/O from backing failures that may have written some bytes.

The checker must calculate expected results independently of the generator and candidate.
Set explicit limits on retained histories and uncertain operations. If a limit is reached, stop
and mark the run incomplete. Do not discard information that could change the expected result.
The checker keeps up to 65,536 rejected writes per run. Before a recovery, it keeps one uncertain
write and one uncertain discard or zero write per LBA. Recovery adds each retired uncertain write to
the rejected writes: after recovery, that payload is never a valid result.

The checker's model uses about 36 bytes per LBA: about 38 MiB at the 4 GiB scale. The harness
runs outside the daemon's cgroup, so the memory gate does not count this model.

These checks treat each LBA separately. They do not verify record order across LBAs or internal
checkpoint structures; ADR-05's engine checker covers those rules.

### What happens after a crash?

Verify the recovered state before issuing new writes:

1. Save the stopped daemon's backing image before reopen.
2. Reopen the device and read every LBA without issuing writes or discards.
3. Record all observed values and checker violations.
4. If the case continues, rewrite the latest intended contents with new versions and flush.

Step 4 establishes a known state for the next test phase. It does not repair the engine or prove
that recovery was correct. Keep the observations and verdict from before those writes.

### How do we test the checker?

Run these fixed tests against memory targets before random testing. The target deliberately returns
wrong data in the four bug-detection cases:

| Test sequence | Required result |
|---|---|
| Run a correct target without faults. | Checks pass before and after recovery. |
| Write v1 and flush. Crash, then read. The target returns the value from before v1. | Detect loss of durable data after recovery. |
| Write LBA A, then read LBA B without a crash. The target returns A's payload. | Detect data returned from the wrong LBA. |
| Write, discard, flush and crash. After recovery, the target returns the pre-discard payload. | Detect discarded data returning after recovery. |
| Flush v1, reject v2 without changing storage, write v3 and crash. After recovery, the target returns v2. | Detect a rejected write becoming visible. |
| Store a write, make its completion uncertain and crash. After recovery, the target returns that write. | Accept the outcome when the target contract permits it. |
| Schedule a fault after the last operation. | Report no violation and a fault count of zero. |
| Start a misdirected-write fault at the first rewrite after recovery. | Report no violation from recovery: every verdict precedes the rewrites. |

`tools/blockcheck/tests/checker_faults.rs` implements each row as one test. A random suite also runs
every fault, and a correct target with injected rejections and I/O errors. For each checker rule,
we deliberately broke the rule and confirmed that a named test failed (2026-10-20).

Each deliberate-bug test must name the fault trigger, affected LBA and expected violation kind.
It must state whether detection occurs before or after recovery, and record whether the fault
occurred. `violation_count > 0` alone is not enough. Where the test can restore a known state,
verify that state separately after the rewrite and flush.

### Random tests and reproduction

Start with short operation sequences over a small working set. Record the limits on sequence
length, LBA count and capacity. Report which state transitions, faults and outcomes were checked.
For each failure, save the operations, completions, flush results, crash positions and faults.
Reduce the sequence to a smaller reproducer and keep it as a regression test.

Before claiming reuse safety, test images that have already undergone repeated reuse and metadata
pressure. ADR-05 separately covers checkpoint corruption, torn backing writes and cleaner scheduling.
A random SIGKILL does not establish coverage of those cases.

## Cases

| Case | Procedure | ADR-08 clause |
|---|---|---|
| `aged-overwrite` | Write the live fraction of the LBAs, flush, then overwrite them until the [stop rule](#endurance-stop-rule) ends the run. Settings: live fraction (1.00 gated, 0.90 measured only), pattern (uniform, hot/cold, moving hot region, sequential), working set (1 LBA to all live LBAs), flush interval. | Cleaning cost; two heads; relocation source check; metadata reclamation |
| `write-pressure` | Write at maximum rate near the space frontier. Then write in bursts with random idle gaps. On ENOSPC, retry with backoff. | Reserve deferral; no ENOSPC with a positive-gain victim; idle cleaning |
| `discard-cycle` | Write a window, flush, discard it, flush, move to the next window. At least 10 cycles and 5 × backing. | Client discard; zeros after discard; discard ordering |
| `crash-restart` | SIGKILL at recorded trace positions during the other cases and reopen, on fresh and aged fixtures. Preserve the image before reopen and verify every LBA without repair. This tests process loss, not power loss. | Device contract; invariants 1–3 |
| `harness-check` | v1.2.3 must reach finite-log ENOSPC under `aged-overwrite` and reject unsupported discard cleanly. Memory targets must pass the exact positive/negative controls before random faults. | None; it tests the harness |

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

### Endurance stop rule

An `aged-overwrite` run writes at least the minimum multiple of the backing. It then continues until
steady state or the maximum multiple, whichever comes first:

| Scale | Minimum | Maximum |
|---|---:|---:|
| 128 MiB | 20 × backing | 25 × backing |
| 4 GiB | 3 × backing | 10 × backing |

ENOSPC before the stop fails the run. A run that reaches the maximum without steady state is a
finding, not a failure. At 4 GiB, 3 × backing reuses each segment about three times at fill factor 0.67.
That may end before cleaning settles, so the run continues past the minimum.

The live fraction is the share of LBAs that preconditioning writes. Unwritten LBAs cost no backing
space, so a live fraction of 0.90 lowers the fill factor to `0.90 × logical / backing`. The
mapping index still scales with the logical size. The 0.90 runs are measured only: ADR-08 makes no
separate promise for them.

## Backing sizes

Two ratios describe how full a device is:

| Term | Ratio | Meaning |
|---|---|---|
| Live fraction | live data ÷ logical size | How much of the volume the client uses. |
| Fill factor | live data ÷ backing size | How much room the cleaner has. ADR-08 uses this term. |

The `Fill` column below is the fill factor. With live fraction 1.00, live data equals the logical
size. Discarded and unwritten LBAs are not live data.

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

## Aged fio and SQLite cases

These two cases add sustained load from standard tools. They do not use the checker: `aged-fio`
measures device performance, and `aged-sqlite` checks database and filesystem integrity. Both use
the same daemon adapters, sampler, memory cap, stall limits and stop rule as the other cases. Each
runs on the candidate's default segment size only.

v1.2.3 runs both cases on the same backing until its log is full. We report the bytes it wrote
before its first error, as a multiple of the backing size. That error is expected and does not
count as a failure.

### `aged-fio`

The device has 4 GiB logical size and 6 GiB + 8 KiB backing (fill factor 0.67, a gated row). The
procedure is:

1. Write every LBA sequentially, then flush (live fraction 1.00).
2. Run fio with `rw=randwrite bs=4k direct=1 ioengine=io_uring iodepth=32 fsync=256`.
   Run once with uniform addresses and once with `random_distribution=zipf:1.2`.
3. Run fio in windows of one backing size each. Between windows, apply the
   [stop rule](#endurance-stop-rule).

fio logs bandwidth and latency per second (`log_avg_msec=1000`). The sampler reads device writes
from `/sys/block/ublkbN/stat` and backing writes from the backing LV's `/sys/block/dm-N/stat`.

### `aged-sqlite`

The device has 4 GiB logical size and 4 GiB + 8 KiB backing. The client uses 75% of the volume,
so live data is about 3 GiB and the fill factor is about 0.75. ext4 must discard freed blocks;
otherwise every block that ext4 ever used stays live. The procedure is:

1. Format the device with `mkfs.ext4`, then run `tune2fs -o discard`. The existing mount helper
   mounts without options, and ext4 applies this superblock default. A loop-device test on
   2026-10-20 confirmed that deleted files then release space.
2. Mount the device through the existing mount helper.
3. Load go-ycsb SQLite records with 4 KiB values. Calibrate `recordcount` so that ext4 reports
   about 3 GiB used after the load.
4. Repeat the run phase of workload A (50% reads, 50% updates). Use a separate fresh device for
   workload F (50% reads, 50% read-modify-write). Count progress in device bytes written, and
   apply the [stop rule](#endurance-stop-rule) between run phases.
5. Run `PRAGMA integrity_check` and compare the record count with the loaded count.
6. Unmount and run `e2fsck -fn`.
7. Stop the daemon cleanly, serve the same backing again, mount it and repeat step 5.

`findmnt` does not show the superblock default. The device's discard fields in
`/sys/block/ublkbN/stat` prove that discards reached the device.

The gate assumes at most 3136 MiB of live data: at that size, 4 GiB of backing is at least
1.25 times the ADR-08 floor for 8, 16 and 32 MiB segments. The sampler records ext4 used space
every second. If it exceeds 3136 MiB, the run is outside the gate and is reported as measured only.

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

## Gates

| Gate | Pass condition |
|---|---|
| Correctness | Candidate runs have zero checker violations. Deliberate-bug tests detect their expected violations. Report v1.2.3's expected ENOSPC and unsupported-discard results separately. |
| Endurance | `aged-overwrite` with live fraction 1.00 reaches its [stop rule](#endurance-stop-rule) without ENOSPC, on every gated size. |
| Discard | `discard-cycle` completes without ENOSPC; discarded LBAs read zeros after FLUSH and restart. |
| Exhaustion | Real exhaustion returns ENOSPC, not EIO or a hang; flushed data survives; writes resume after discard. |
| Memory | No OOM under the [memory cap](#how-is-the-memory-cap-set); peak memory has no upward trend. |
| Stalls | No request takes longer than 1 s under `write-pressure`, excluding ENOSPC retries. ENOSPC retries follow [ENOSPC under pressure](#enospc-under-pressure): backoff at most 1 s, 30 s total. |
| Aged fio | Both `aged-fio` runs reach the stop rule without ENOSPC or EIO. |
| Aged SQLite | Both `aged-sqlite` runs reach the stop rule without ENOSPC, EIO or a read-only remount. `PRAGMA integrity_check` passes and the record count matches before and after the restart. `e2fsck -fn` reports no errors. The device's discard count is greater than zero. |

Performance has no gate. ADR-08 states that fresh-volume throughput cannot gate this design, and
the candidate enables overwrite-heavy workloads that format 3 cannot sustain. That capability is
expected to cost some throughput.

### How is the memory cap set?

```text
memory.max = round_up(2 × P_base + 16 MiB, 16 MiB)
```

`P_base` is v1.2.3's `memory.peak` on the same case, scale, write buffer and cache settings, on
this host. The factor of 2 covers state that format 4 adds: two heads, segment state and the older
index that a cleaner view can retain (ADR-08, "Which metadata stays resident?"). The fixed 16 MiB
covers compaction work in flight. It does not scale with the volume, because read windows and
relocation batches do not.

The 16 MiB is an estimate, not a measurement. ADR-08 requires a streaming cleaner: bounded read
windows, batches sent to the writer by reference, and no retained whole victims. One relocation
`Data` record holds at most 338 × 4 KiB, about 1.3 MiB. The allowance covers a read window plus
about 12 batches in flight.

There is no upward trend when the highest peak in cycles 6 to 10 is at most 1.05 × the highest
peak in cycles 1 to 5.

We measure memory from outside the process: cgroup `memory.peak`, sampled `smaps_rollup` and the
thread count. v1.2.3's idle RSS is the reference for memory outside the heap. The candidate's own
heap reports are diagnostics.

The cgroup also charges page cache. The backing uses O_DIRECT, so we assume this adds little. The
noise baseline must confirm that assumption.

## What do we report without a gate?

| Measurement | Report |
|---|---|
| Fresh comparison | A reproducible throughput loss above 10% or a P99 increase above 20% is a finding. A throughput loss above 50% or a P99 increase above 2× is a severe finding. Neither fails the candidate. |
| Compaction memory | Candidate peak minus `P_base`, compared with the 16 MiB allowance. If the candidate's `--help` exposes heap or eviction budgets, we set them explicitly and report peak memory against them. |
| Memory spike | Candidate peak minus `P_base` of at least one segment size is a finding. It suggests that the cleaner retains a whole victim, which ADR-08 forbids. Other causes are possible, so the finding needs diagnosis. |
| Steady state | Whether each `aged-overwrite` run reached steady state before its maximum multiple. |
| Live fraction 0.90 | Endurance, write amplification and latency, beside the gated 1.00 runs. |

### Which baseline do these numbers need?

Earlier performance results come from other hardware. Before the first candidate run, we run the
fresh comparison for v1.2.3 against itself on this host. If that noise exceeds the 10% or 20%
trigger, we revisit the trigger before the candidate run. The same runs measure `P_base`.

### Why these values?

- **Regression trigger:** the implementation prompt's 10% and 20% replace the template's 90% and
  110%. We expect P99 on ublk to vary more than throughput. This is not measured; the noise
  baseline checks it.
- **Stall limits:** 1 s is far above normal device latency, and a cleaner wait of seconds is a real
  defect. 30 s matches the request timeout we expect from ublk. Read
  `/sys/block/ublkbN/queue/io_timeout` to confirm it.
- **Endurance:** a fixed multiple can end before cleaning settles. A stop rule that continues to
  steady state, with a maximum, bounds the runtime and still measures settled behavior.

## Evidence

```text
evidence/blockcheck-<run-id>/
  manifest.json          versions, SHAs, dirty state, binary hashes, format, seed, kernel, CPU, backing, bounds, frozen gates
  <case>/case.json       settings, expected results, verdicts, faults that occurred, coverage
  <case>/trace.jsonl     issued operations, completions, flush results, crash/fault positions
  <case>/timeline.csv    per second: ops, latency percentiles, device counters, st_blocks, memory
  <case>/daemon.log      daemon stderr
  <case>/failure/        image before reopen, observations before rewrites, reduced sequence
  summary.json
```

Keep verification, rewrites and image capture outside timed performance intervals.
Reports must distinguish:

- checker violations;
- expected rejections, such as finite-log ENOSPC or unsupported discard;
- expected errors from the selected medium fault;
- failures of the test harness itself.

If the run cannot show that an intended fault occurred, its fault coverage is incomplete.
The same applies if the run exceeds a resource or time limit before finishing its planned cases.

## Deliveries

1. Define allowed recovery results and pass the fixed checker tests. Done on 2026-10-20. Then add
   the generator and device target. The untracked library is not device acceptance evidence.
2. Add the v1.2.3 adapter and sampler. Verify its expected ENOSPC and unsupported-discard behavior
   with `aged-overwrite` and `harness-check`. Calibrate against `fio` and measure XFS overhead against
   the raw LV.
3. Add the remaining cases, sequence replay and reduction, backing-size sweep and ADR-10 parser.
   Freeze the gates before judging the reclamation candidate. Record its commit and binary identity.
4. Add `aged-fio` and `aged-sqlite`, using the adapters from delivery 2. Calibrate the SQLite
   `recordcount` on a 4 GiB host ext4 image before the candidate run. v1.2.3's log may fill
   during the load itself.
5. Connect ADR-05's simulated backing to the logical checker and its separate structural and
   record-order checks. Run it in process first, then as an unprivileged ublk backing target.
