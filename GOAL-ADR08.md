# GOAL-ADR08: implement ADR-08 (format 4, raw reclamation, cleaner thread)

Branch `implement/adr08` in the private clone `/home/miguel/play/_worktrees/msfblkdev-adr08`,
started from `57d56cc`. Build directory: `rust/target` inside this clone. Evidence:
`evidence/adr08-implementation/`. This file is the durable handoff across restarts.

## Checklist

Phase 1: green baseline and contracts
- [x] Baseline `make lint test` at `57d56cc`: PASS, 44.9 s (`evidence/adr08-implementation/baseline-lint-test.log`).
- [x] Baseline `make test-acceptance` at `57d56cc`: PASS, 313 tests, 96.6 s (`evidence/adr08-implementation/baseline-test-acceptance.log`; the gitignored engine-crash log is named inside it).
- [x] Format-4 codecs: header/kinds, segment table page, mapping index page, link, growth, discard, descriptor (`a462b96`).
- [x] Geometry, segment allocator, accounting model (`cffdf23`).
- [x] Mapping index tree, checkpoint load, planned publication, format (`2b17771`).
- [x] Two-head replay, tail probing, sanitation, recovery windows (`2b17771`); SIGKILL boundaries for
      publication, recovery and stale-tail clearing run through the ported `process_crash`,
      `checkpoint_batches`, `packed_writes` and `stale_tail` matrices.
- [x] Engine on format 4; format-3 images rejected; behavioral tests ported; daemon `format --segment-bytes`.
      Gate: `make lint test` PASS, 304 tests (`evidence/adr08-implementation/gate-engine.log`).

Phase 2: serialized raw relocation and reuse
- [x] Victim scan through fixed windows, conditional apply on the Relocation head, snapshot copy or
      dirtying, index/table page dirtying, two-publication barrier, hole punch after release.
- [x] Wraparound (3x capacity with interleaved cold payloads; 35+ segments freed), fallback after
      reuse (T7), stale and reused-source rejection (T1), discard versus queued relocation,
      backing growth with SIGKILL boundaries, all-live ENOSPC, zero allocations in steady state,
      SIGKILL matrix at relocation/link/discard/publication boundaries with a prefix oracle
      (`rust/src/tests/reclaim.rs`, `reclaim_crash.rs`). Commit `0d41533`; gate
      `evidence/adr08-implementation/gate-reclaim.log`; acceptance PASS, 314 tests
      (`acceptance-0d41533.log`).

Phase 3: cleaner thread and scheduling
- [x] Cleaner thread (`format_v4/reclaim/cleaner.rs`): own duplicate descriptor and ring; window
      buffers, views and cursors move through bounded channels; at most one scan, one view swap and
      four discards outstanding; eventfd wake-up; shutdown joins after the current command.
- [x] Writer state machine (`volume/reclaim.rs`): `maintain` never blocks (`Scanning`/`Waiting`
      steps), `wait_for_cleaner` blocks on one report, pressure blocks instead of spinning,
      `maintain_for(mode, slice)` for request loops; hole punches run on the cleaner thread.
- [x] `Scheduler` (`reclaim/scheduler.rs`): injectable instants, idle wait 2 ms distinct from the
      100 ms maximum interval, lateness accounting.
- [x] Tests (`tests/cleaner_thread.rs`): delayed reads wake through the descriptor without writer
      I/O or spinning, pressure waits on the report, close with a read outstanding, injected read
      failure latches, scheduled idle and due ticks reduce pressure reclamation.
- [x] Daemon: timer and eventfd poll on the queue ring as classified target I/O (`extra_ios = 2`),
      discard and write-zeroes (`UBLK_PARAM_TYPE_DISCARD`), heap/cleaner/growth/discard/scheduling
      flags, SIGUSR1 claims an operator-grown backing, statistics at close.

Phase 4: gates, live acceptance, performance, ADR update, tag `v2.0-rc`.
- [x] ublk-fio lab on format 4 (`8af23f1`): PASS, 8 scenarios × 3 repetitions in 44 s, including
      sustained overwrite that appended 2544 blocks into a 320-block log and freed 74 segments,
      and thin-volume exhaustion at 182–190 written blocks
      (`evidence/adr08-implementation/ublk-fio-8af23f1.log`, full lab log beside it).
- [ ] `make test-ext4`, `make test-xfs`, SQLite matrices: UNAVAILABLE in this clone. The lab's
      preflight requires the installed root-owned mount helper to be byte-identical to the
      helper built from the current tree, and installing helpers is not authorized here
      (`check-ext4-8af23f1.log`, `check-xfs-8af23f1.log`). Reported as unverified.
- [x] Acceptance on the candidate `2303843`: PASS, 326 tests, 85 s
      (`evidence/adr08-implementation/acceptance-2303843.log`).
- [x] Performance comparisons (`tools/adr08-bench/`): baseline `57d56cc` against the candidate,
      3 interleaved repetitions, 72 runs each on a btrfs fixture (`perf/report-btrfs.md`) and on
      tmpfs (`perf/report-tmpfs.md`); summarized in `ADR-08-PERF-RESULTS.md`.
- [x] ADR-08 implementation decisions, review dispositions and README notes (`2303843`).
- [x] Final read-only review of the complete diff: three blockers found and fixed (decision 22),
      idle watermark added (decision 23), unused `CleanerShape::proposal_payloads` removed,
      `maintenance_ready` wired into the daemon's timer arming.
- [ ] Gates on the final candidate, re-measured performance, tag `v2.0-rc`.

## Decisions (resolved draft gaps)

See the "Implementation decisions" section of ADR-08 once written. Working list:

1. Frame derivation: a record starts at `previous_boundary + 1` when both are in one segment, else at
   the start of the segment holding `boundary`. `previous_boundary = 0` means "no earlier record on
   this head". Payload count is `boundary - start`; non-data records require `boundary == start`.
2. Bootstrap: format opens segment 0 for User and segment 1 for Relocation, appends the initial
   segment table pages to segment 0, then writes both descriptors. A head's open segment comes from
   the segment table (`Open` + head bit), never from the boundary.
3. Freshness (T2): the writer fsyncs a `SegmentLink` before appending to the segment it names. Replay
   accepts a record only when its LSN equals the expected global LSN, which exceeds every LSN in any
   free segment. Sanitation zeroes the bounded window after each head's recovered position.
4. Source identity (T1): a proposal names `(victim, seal_lsn)`. The writer applies it only while the
   victim is still `Sealed` with that `seal_lsn`, the mapping still points at the source block and no
   buffered write exists for the LBA.
5. Fallback after reuse (T7): replay lets a `SegmentLink` open a `Free` or `PendingFree` segment.
6. Publication allocation (T4): snapshots and index pages append first; the writer then advances the
   User head if the dirty table pages do not fit, and appends the table pages last so they describe
   the final allocation. One fsync precedes the descriptor write; one follows it.
7. Metadata relocation (T8): the writer re-emits current index/table pages and resident snapshots
   itself by marking them dirty; non-resident clean snapshots are copied from cleaner proposals.
8. Eligibility: a sealed segment is off both replay paths when `seal_lsn <= min(root LSNs)`.
9. Sanitation zeroes the rest of each head's open segment, not a 339-block window: a torn maximal
   record can hide a complete successor beyond 339 blocks inside the same segment, and the
   successor would chain onto a same-sized re-issued record after reopen.
10. Heap budget: `total = max(logical × proportion, 16 MiB floor)`; the write buffer, cleaner and
    cache requests are validated against it and never enlarged; the cleaner share is at least one
    read window plus one proposal buffer so write-through configurations still open.
11. Reservation: `N + R + D + P` plus one footer per possible segment crossing is demanded from
    the User head's room plus the free segments above the reserve. The reserve is one worst-case
    victim relocation plus two publications, each planned from a full head.
12. Discard drops buffered payloads in range first, chunks the range so every touched page fits
    the cache, publishes before appending when the chunk would not fit, then appends one
    `LbaDiscard` and unmaps with checkpoints forbidden, so no root can pass the record's LSN with
    pages still mapped. Write-zeroes is a discard.
13. Progress gate: maintenance starts a victim only when its copies fit the Relocation head's
    room plus allocatable segments and two publications fit the User head's room plus what
    remains; a publication that cannot fit fails with `ENOSPC` without latching. While a victim
    waits for its second publication, admission also reserves that publication's blocks.
14. Views: the scanner reads liveness against a copy of the resident index taken at the newest
    root. An eligible victim (seal LSN at or below both roots) has every write into it reflected
    there, so a payload the view calls dead never becomes live again; live-in-view payloads are
    re-checked at apply against the cache and the buffered writes.
15. Victims that a completed scan could not empty (for example a live payload whose bytes fail
    their checksum) are marked stuck and skipped until a mapping change touches them.
16. Cleaner ownership: the cleaner thread holds a duplicate backing descriptor, its own ring, the
    scan scratch and one view. Window buffers travel writer → cleaner → writer with the scan
    cursor as the token, so one victim has one window in flight while the writer applies the
    previous one; the writer keeps a spare view, fills it from the resident index when a victim
    is selected and swaps it in ahead of the victim's first window (commands are ordered).
    Nothing is shared under a lock.
17. Scheduling: `maintain` never blocks. The daemon keeps one timeout SQE at the scheduler's wait
    hint and one poll SQE on the cleaner's eventfd in the queue ring, classified as target I/O
    by operation code before any tag indexes a buffer. A tick runs when no request completed for
    the idle wait (2 ms) or when the maximum interval (100 ms) passed under continuous requests,
    and runs steps for at most one idle wait plus one step. Pressure reclamation inside a write
    blocks on the cleaner's report (60 s bound) instead of spinning.
18. Backing discards run on the cleaner thread after the freeing publication, at most four in
    flight; a segment stays excluded from allocation until its punch completes.
19. Idle cleaning policy: a victim is worthwhile while idle only when at least a quarter of its
    blocks are dead (`idle_minimum_dead`), because every sealed segment carries a dead link and
    dead footers and idle cleaning would otherwise copy live data from one packed segment to the
    next forever. Idle cleaning also starts a victim only when its copies and two publications
    fit without touching the foreground reserve; pressure cleaning may spend the reserve. Found
    by a probe in which idle maintenance after every write-through record drove a 64-block
    volume to `ENOSPC` after 38 writes; with both rules 4096 writes and 12 full overwrite passes
    complete (`tests/cleaner_thread.rs`).
20. Emptied segments need no scan: when a user or relocation remap removes the last live
    payload from a sealed segment that holds no current metadata, the writer marks it
    `PendingFree` at once (`Segments::release_if_empty`) and the barrier frees it. Metadata
    moves do not do this, because they run inside a publication after its table images were
    built, and a state change there would desynchronize the in-memory record of the durable
    root (found by the fallback-after-reuse test). Victims whose last payloads had buffered
    overwrites at apply time are therefore released by the release of those writes instead of
    staying stuck; before this rule a quarter-full 128 MiB volume under random overwrite
    accumulated 23 stuck victims whose copies had consumed the reserve and hit `ENOSPC`.
21. Pressure gates count publications at the reserve's worst-case size, not the current dirty
    set, because relocation copies dirty mapping pages and enlarge the publications that must
    follow. When nothing is left to clean while writes are buffered, pressure releases the
    buffer once before returning `ENOSPC`, since buffered overwrites keep victims' old payloads
    live until they land. A segment reused before its punch was offered drops its discard mark.
22. Findings of the final read-only review, all fixed with regression tests: the cleaner thread
    kept the geometry it was spawned with, so the first scan or discard in a grown range would
    have latched the volume (now `Command::Geometry` after `grow_backing` and after replay);
    a victim whose scan left only resident current metadata behind stayed stuck forever (now
    `move_metadata` clears the mark and a publication that empties a sealed segment releases
    it, as does open for segments the last publication emptied); the daemon's early-returning
    tick never drained the cleaner's eventfd, so the re-armed poll completed at once and the
    queue loop spun until the deadline (90,414 wake-ups for 156 ticks in the first ublk-fio
    log; now the tick drains and applies reports before returning).
23. Idle cleaning rests while at least half the log's segments are free
    (`idle_watermark`, never below the reserve plus one): every barrier publication leaves
    stale table pages behind, and on small segments those alone re-create victims, so idle
    cleaning without a free-space floor fed itself forever. Pressure cleaning has no floor of
    its own because `reclaim_for_space` stops when the write fits. Pressure cleaning that finds
    no startable victim while a backing discard is in flight waits for it instead of returning
    `ENOSPC` (the first live run after the review fixes stopped at 102 blocks with a freed
    segment still being punched); the lab's exhaustion floor is the no-compaction bound of 77
    write-through records. With the wait in place the all-live loop compacted forever (each freed
    segment consumed again by copies and barrier pages), so pressure reclamation now returns
    `ENOSPC` at once when live payloads, packed footers, current metadata and the demanded blocks
    cannot fit the segments cleaning could ever free, backed by a stall bound of one freeing
    cycle per segment.
## Commands

```sh
cd /home/miguel/play/_worktrees/msfblkdev-adr08 && export CARGO_TARGET_DIR="$PWD/rust/target"
make lint test                 # required gate before each implementation commit
make test-acceptance           # exhaustive crash matrix (~10 min at baseline)
```

## Status

Last green commit: see `git log`; every implementation commit carries its `make lint test` log
under `evidence/adr08-implementation/`. Acceptance: PASS on `0d41533` (314 tests) and on
`2303843` (326 tests). Live: ublk-fio PASS on `8af23f1`; ext4, XFS and SQLite gates unavailable in
this clone (mount helper identity). Remaining: performance report write-up, final diff review, tag
`v2.0-rc`.
