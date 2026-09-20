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
- [ ] Victim scan, conditional apply, metadata re-emission, two-publication barrier, actual reuse.
- [ ] Wraparound, fallback after reuse, discard, backing growth, bounded allocation.

Phase 3: cleaner thread and scheduling
- [ ] Bounded proposal storage, buffer ownership, eventfd wake-up, deterministic scheduler tests.

Phase 4: gates, live acceptance, performance, ADR update, tag `v2.0-rc`.

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

## Commands

```sh
cd /home/miguel/play/_worktrees/msfblkdev-adr08 && export CARGO_TARGET_DIR="$PWD/rust/target"
make lint test                 # required gate before each implementation commit
make test-acceptance           # exhaustive crash matrix (~10 min at baseline)
```

## Status

Last green commit: the engine switch (see `git log`). Dirty work: none. Known debt: a module-level
`expect(dead_code)` in `rust/src/format_v4.rs` covers items the reclamation commit uses; remove it
there. The ublk-fio lab still assumes the format-3 fixture geometry and must move to a format-4
fixture in phase 4. Next action: phase 2, serialized raw relocation (victim scan, conditional
apply, metadata re-emission, two-publication release, actual reuse under repeated overwrites).
