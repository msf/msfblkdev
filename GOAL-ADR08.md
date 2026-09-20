# GOAL-ADR08: implement ADR-08 (format 4, raw reclamation, cleaner thread)

Branch `implement/adr08` in the private clone `/home/miguel/play/_worktrees/msfblkdev-adr08`,
started from `57d56cc`. Build directory: `rust/target` inside this clone. Evidence:
`evidence/adr08-implementation/`. This file is the durable handoff across restarts.

## Checklist

Phase 1: green baseline and contracts
- [x] Baseline `make lint test` at `57d56cc`: PASS, 44.9 s (`evidence/adr08-implementation/baseline-lint-test.log`).
- [x] Baseline `make test-acceptance` at `57d56cc`: PASS, 313 tests, 96.6 s (`evidence/adr08-implementation/baseline-test-acceptance.log`; the gitignored engine-crash log is named inside it).
- [ ] Format-4 codecs: header/kinds, segment table page, mapping index page, link, growth, discard, descriptor.
- [ ] Geometry, segment allocator, accounting model.
- [ ] Mapping index tree, checkpoint load, planned publication, format.
- [ ] Two-head replay, tail probing, sanitation, recovery windows, crash boundaries.
- [ ] Engine on format 4; format-3 images rejected; existing behavioral tests ported.

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

## Commands

```sh
cd /home/miguel/play/_worktrees/msfblkdev-adr08 && export CARGO_TARGET_DIR="$PWD/rust/target"
make lint test                 # required gate before each implementation commit
make test-acceptance           # exhaustive crash matrix (~10 min at baseline)
```

## Status

Last green commit: `57d56cc` (baseline). Dirty work: none. Next action: format-4 codecs.
