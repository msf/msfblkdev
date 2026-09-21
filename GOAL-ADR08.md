# GOAL-ADR08: implement ADR-08 (format 4, raw reclamation, cleaner thread)

This file is the implementation record and the durable handoff across restarts.

- Branch: `implement/adr08` in the private clone `/home/miguel/play/_worktrees/msfblkdev-adr08`,
  started from `57d56cc`.
- Build directory: `rust/target` inside the clone.
- Evidence: `evidence/adr08-implementation/`.
- Result: complete. The tag `v2.0-rc` marks the final candidate `8c7f650`.

## Status

The work is complete. Every implementation commit carries its `make lint test` log under
`evidence/adr08-implementation/`.

| Gate | Result | Evidence |
|---|---|---|
| `make lint test` | PASS on every commit. 328 tests at the tag. | `gate-*.log` |
| `make test-acceptance` | PASS on `0d41533` (314 tests), `2303843` (326), `8e7e0e2` (328) and `8c7f650` (328). | `acceptance-<commit>.log` |
| `make test-ublk-fio` | PASS on `8af23f1` and `8c7f650`. The run on `8e7e0e2` failed on the lab's old exhaustion floor. That log is kept as evidence of the defect that `8c7f650` fixed. | `ublk-fio-<commit>.log` and the full lab log beside each |
| `make test-ext4`, `make test-xfs`, SQLite matrices | Not run. Unverified. | `check-ext4-8af23f1.log`, `check-xfs-8af23f1.log` |

Why the filesystem gates did not run: the lab's preflight requires the installed root-owned mount
helper to be byte-identical to the helper built from the current tree. Installing helpers was not
authorized in this clone.

Two tests outside this work are flaky under the parallel test run. Both pass alone:

- `io_accounting::tests::new_writer_refuses_abandoned_pending_work_without_repair` returns
  `WouldBlock` under parallel load.
- `ublk_fio::tests::cleanup_refuses_changed_identity_and_preserves_backing` collides on an
  evidence file name when two tests start in the same nanosecond.

## Checklist

### Phase 1: green baseline and contracts

- [x] Baseline `make lint test` at `57d56cc`: PASS in 44.9 s
      (`evidence/adr08-implementation/baseline-lint-test.log`).
- [x] Baseline `make test-acceptance` at `57d56cc`: PASS, 313 tests in 96.6 s
      (`baseline-test-acceptance.log`; it names the gitignored engine-crash log).
- [x] Format-4 codecs: header and kinds, segment table page, mapping index page, link, growth,
      discard and descriptor (`a462b96`).
- [x] Geometry, segment allocator and accounting model (`cffdf23`).
- [x] Mapping index tree, checkpoint load, planned publication and format (`2b17771`).
- [x] Two-head replay, tail probing, sanitation and recovery windows (`2b17771`). The ported
      `process_crash`, `checkpoint_batches`, `packed_writes` and `stale_tail` matrices run the
      SIGKILL boundaries for publication, recovery and stale-tail clearing.
- [x] Engine on format 4. Format-3 images are rejected. Behavioral tests are ported. The daemon
      has `format --segment-bytes`. Gate: `make lint test` PASS, 304 tests (`gate-engine.log`).

### Phase 2: serialized raw relocation and reuse

- [x] Victim scan through fixed windows. Conditional apply on the Relocation head. Snapshot copy
      or dirtying. Index and table page dirtying. Two-publication barrier. Hole punch after
      release.
- [x] Tests in `rust/src/tests/reclaim.rs` and `reclaim_crash.rs`:
      - Wraparound: 3x capacity with interleaved cold payloads, 35 or more segments freed.
      - Fallback after reuse (T7).
      - Stale and reused-source rejection (T1).
      - Discard against a queued relocation.
      - Backing growth with SIGKILL boundaries.
      - All-live `ENOSPC`.
      - Zero allocations in steady state.
      - SIGKILL matrix at the relocation, link, discard and publication boundaries with a prefix
        oracle.
- [x] Commit `0d41533`. Gate `gate-reclaim.log`. Acceptance PASS, 314 tests
      (`acceptance-0d41533.log`).

### Phase 3: cleaner thread and scheduling

- [x] Cleaner thread (`format_v4/reclaim/cleaner.rs`): own duplicate descriptor and ring. Window
      buffers, views and cursors move through bounded channels. At most one scan, one view swap
      and four discards are outstanding. An eventfd signals each report. Shutdown joins after the
      current command.
- [x] Writer state machine (`volume/reclaim.rs`): `maintain` never blocks and reports `Scanning`
      or `Waiting` steps. `wait_for_cleaner` blocks on one report. Pressure blocks instead of
      spinning. `maintain_for(mode, slice)` serves request loops. Hole punches run on the cleaner
      thread.
- [x] `Scheduler` (`reclaim/scheduler.rs`): injectable instants, a 2 ms idle wait distinct from the
      100 ms maximum interval, and lateness accounting.
- [x] Tests (`tests/cleaner_thread.rs`): delayed reads wake through the descriptor without writer
      I/O or spinning. Pressure waits on the report. Close works with a read outstanding. An
      injected read failure latches. Scheduled idle and due ticks reduce pressure reclamation.
- [x] Daemon: timer and eventfd poll on the queue ring as classified target I/O (`extra_ios = 2`).
      Discard and write-zeroes (`UBLK_PARAM_TYPE_DISCARD`). Flags for heap, cleaner, growth,
      discard and scheduling. `SIGUSR1` claims an operator-grown backing. Statistics at close.

### Phase 4: gates, live acceptance, performance, ADR update, tag `v2.0-rc`

- [x] ublk-fio lab on format 4 (`8af23f1`): PASS, 8 scenarios × 3 repetitions in 44 s. Sustained
      overwrite appended 2544 blocks into a 320-block log and freed 74 segments. Thin-volume
      exhaustion came at 182 to 190 written blocks (`ublk-fio-8af23f1.log`).
- [ ] `make test-ext4`, `make test-xfs` and the SQLite matrices: UNAVAILABLE in this clone. See
      Status. Reported as unverified.
- [x] Acceptance: PASS on `2303843` (326 tests), `8e7e0e2` (328) and `8c7f650` (328 tests, 86 s,
      `acceptance-8c7f650.log`).
- [x] ublk-fio on the final candidate `8c7f650`: PASS, 8 scenarios × 3 in 44 s. Wraparound
      appended 1810 blocks into a 320-block log and freed 53 segments. Exhaustion came at 146
      written blocks. The daemon ran 615 maintenance ticks with 177 cleaner wake-ups across its
      lifetimes (`ublk-fio-8c7f650.log`).
- [x] Performance comparisons (`tools/adr08-bench/`): baseline `57d56cc` against the candidate,
      3 interleaved repetitions, 72 runs each on a btrfs fixture (`perf/report-btrfs.md`) and on
      tmpfs (`perf/report-tmpfs-8c7f650.md` for the tagged commit, earlier runs beside it).
      [ADR-08-PERF-RESULTS.md](ADR-08-PERF-RESULTS.md) summarizes them.
- [x] ADR-08 implementation decisions, review dispositions and README notes (`2303843`).
- [x] Final read-only review of the complete diff. Three blockers found and fixed (ADR-08
      decisions 15, 20, 22 and 23). Idle watermark added (decision 19). Unused
      `CleanerShape::proposal_payloads` removed. `maintenance_ready` wired into the daemon's timer
      arming.
- [x] Gates on the final candidate `8c7f650` (lint and test, acceptance, ublk-fio). Performance
      re-measured. Tag `v2.0-rc`.

## Decisions

The decisions live in one place: the
[Implementation decisions](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#implementation-decisions)
section of ADR-08. This index names them:

1. Frame derivation.
2. Bootstrap.
3. Freshness: link fsync before use, exact expected LSN on replay.
4. Checksums: two independent checksums per one-block metadata record.
5. Kind 6 and address bit 31 rejected.
6. Publication allocation: table pages append last.
7. Fallback after reuse: a link may open a `Free` or `PendingFree` segment.
8. Sanitation zeroes the rest of the open segment.
9. Discard ordering; write-zeroes is a discard.
10. Heap budget.
11. Reservation and reserve.
12. Eligibility: `seal_lsn <= min(root LSNs)`.
13. Progress gate.
14. Views.
15. Stuck victims.
16. Cleaner ownership.
17. Scheduling.
18. Backing discards; pressure waits on an in-flight punch.
19. Idle cleaning policy: gain floor, reserve kept, free-space watermark.
20. Emptied segments need no scan.
21. Pressure release and progress: buffer release once, feasibility check, stall bound.
22. Geometry follows growth.
23. Wake-up draining.

## Commands

```sh
cd /home/miguel/play/_worktrees/msfblkdev-adr08 && export CARGO_TARGET_DIR="$PWD/rust/target"
make lint test                 # required gate before each implementation commit
make test-acceptance           # exhaustive crash matrix (~10 min at baseline)
```
