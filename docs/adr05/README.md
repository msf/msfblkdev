# ADR-05: what happens when backing I/O fails?

Authored by LLM. Campaign branch: `adr05/resilience-campaign`, 20 November 2026. **ADR-05 remains open.** The default daemon stops serving I/O after an uncertain backing failure. One library-only retry can recover a transient records-flush failure under a narrower backing contract. It is not a general retry policy or a released daemon feature.

[Explore the fault results](../adr05-report.html) · [Read the decision](../../ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) · [Check the remaining gates](../ADR05-CAMPAIGN.md)

## What did we learn?

| Question | Result and evidence |
|---|---|
| Can we prove the intended backing call failed? | The [syscall probe and engine-close runs](../../evidence/adr05-engine-arm64-20261119/) calibrate the injector. At live ublk checkpoint B, failing either the descriptor write or its final sync returned `EIO`, freed no segments and recovered all eight blocks. The clean control freed six segments. The deliberately wrong-sector rule tests the harness, not the engine. [Live runs and negative control](../../evidence/adr05-live-b-20261119/) |
| Which bugs did the campaign find? | **F01:** backing `ENOSPC` left the volume usable after a failed sync. The fix fences it without poisoning a capacity refusal that happened before I/O. **F02:** valid old metadata at a reused address could prevent reopen after clean close. Recovery now recognizes the validated stale record without accepting damaged metadata. [Triage](../ADR05-TRIAGE-01.md) · [F01](../../evidence/adr05-f01-repair-20261119/) · [F02](../../evidence/adr05-f02-repair-20261119/) · [combined arm64 qualification at `77fc16e`](../../evidence/adr05-repair-qualification-20261119/README.md) |
| How do we check durability? | A [medium model](../../tools/adr05-fault-lab/model/README.md) separates pending bytes from durable bytes. Recovery runs on a copy, compared with caller history kept outside the image. A separate [image checker](../../tools/adr05-fault-lab/checker/) walks both roots. [Modeled record cuts through production recovery](../../evidence/adr05-model-engine-20261119/) and [seeded client histories](../../evidence/adr05-traces-20261119/) are different tests from the backing-fault sweep below. |
| Does a successful second `fsync` restore lost writes? | **No.** In FSYNC-01 both writes disappeared. A bare retry returned client success but recovered both **old** values. The baseline correctly failed closed. With library retry enabled, the engine rewrote both records at their original offsets, synced, then recovered both **new** values. [Baseline](../../evidence/adr05-fsync01-20261120/BASELINE.md) · [ordered replay and limits](../../evidence/adr05-fsync01-20261120/GREEN.md) |
| Can we reproduce a fault from a seed? | Yes. The v1 schedule selects writes, an ordinal and a backing effect for each epoch: reject a write, return a zero-byte write, report an error after a completed write, reject a sync, or report an error after a completed sync. Seeds 0–31 passed **128 epochs** on the default fail-closed engine. Seed 42 produced byte-identical traces in two fresh runs and a saved-plan replay. [Sweep and reproduction](../../evidence/adr05-seeded-dst-20261120/README.md) |

## Run a case

From the repository root, `make test-fault-model` runs the portable model controls (Python 3; no VM). For production recovery, use Linux with direct I/O and `io_uring` on disposable regular files. The recorded native runs used arm64 and Rust 1.97.1. No ublk device or root privilege is needed for these commands:

```sh
make test-dst              # Model controls and checkpoint descriptor survival
make test-fsync01          # Lost-writeback controls and opt-in engine replay
work=$(mktemp -d)
make test-fault-seed SEED=42 OUT="$work/generated"
make test-fault-seed PLAN="$work/generated/plan.txt" OUT="$work/replayed"
cmp "$work/generated/trace.txt" "$work/replayed/trace.txt"
```

The seed runner checks the injected call, client error, fence and all eight logical blocks after reopen. Keep the plan and trace with the tested revision. A missed fault is **inconclusive**. A successful negative control means the harness detected the planted mistake, not that the engine survived that fault.

For QEMU/FUSE and live ublk tests, use the [prepared guest procedure](../../tools/adr05-fault-lab/README.md) and the [checkpoint-B runbook](../../tools/adr05-fault-lab/live/README.md). The [operator-only medium tier](../../tools/adr05-fault-lab/operator/) requires separate approval and disposable storage. Do not substitute a host device.

## What is still open?

FSYNC-01 assumes that a successful barrier covers its epoch. If the backing reports an earlier epoch’s writeback error late, replaying the current epoch cannot repair it. Keep retry off on unqualified backing stacks. The seeded runner preserves completed writes at its recovery cut; it does not simulate power loss, kernel scheduling or cleaner interleavings.

The [campaign plan](../ADR05-CAMPAIGN.md) tracks retry ownership across timeouts and cancellation, positive-length partial transfers, checkpoint and cleaner faults, crashes during retry, other native configurations, and operator-run filesystem and database tests. The existing evidence does not establish PostgreSQL recovery or physical power-loss durability.
