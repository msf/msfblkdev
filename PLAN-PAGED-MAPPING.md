# Delivery plan: paged mapping and bounded cache

Status: Deliveries 1–2 complete; checkpoint publication and bounded recovery verified in isolation; public engine integration remains pending
Contract: [ADR-07](ADR-07-PAGED-MAPPING-AND-CACHE.md)

Delivery 1 evidence (2026-09-18): 16 isolated cache tests passed, including allocation-counted churn and a deterministic mapping reference model. `make lint test` passed all 234 test commands. The cache is compiled only under `cfg(test)`; persistence and engine integration remain unimplemented.

Delivery 2 evidence (2026-09-18): isolated framing, mapping snapshots, data footers, checkpoint descriptors and index bodies, formatter, checkpoint loading, append reservation, and buffered-page grouping pass their tests. The latest `make lint test` passed all 265 commands. ADR-07 records each step's validation. Corrupted-snapshot traversal and bounded recovery publication ordering remain open; the public engine still uses version 1.

Delivery 3 prerequisite (2026-09-18): isolated checkpoint publication appends dirty snapshots and persists the inactive index and descriptor before selecting the new root or clearing dirty flags. Three tests cover successful publication, pointer reuse, preflight rejection, and failures at each write stage. `make lint test` passed all 268 commands. Integration and crash/recovery proof remain pending.

Publication crash evidence (2026-09-18): the existing SIGKILL harness passed 160 interruptions across eight version-3 publication boundaries. The selected root and the flushed data record matched expectations after every crash. All 269 repository gate commands passed. Recovery integration and its interruption tests remain pending.

Recovery framing decision (2026-09-18): ADR-07 now uses eight reserved snapshot bytes for a checksum of the header and page ID. This lets replay traverse a damaged snapshot body while rejecting it as checkpoint mapping state. Byte-by-byte corruption and malformed-header tests passed; all 271 repository gate commands passed. The remaining prerequisite is bounded recovery ordering and integration.

Tail-reader evidence (2026-09-18): four file-backed tests cover mixed records, record-size limits, partial data appends, malformed/unsupported framing, and older-root traversal through damaged snapshot contents to later flushed data. All 275 repository gate commands passed. Cache-window application and recovery publication remain pending.

Bounded-recovery evidence (2026-09-18): isolated cache-window replay matches the fitting-cache path, preserves replay-source bytes, publishes one final root, and rejects insufficient space before writing. Recovery keeps at most one cache plus the active and candidate indexes. All 278 repository gate commands passed. Recovery crash tests, public engine activation, metrics, and final acceptance remain pending.

Recovery crash evidence (2026-09-18): 180 interruptions across nine recovery boundaries and three consecutive interrupted recoveries of one image passed. All final mappings and payload checksums matched the original 338 writes. The combined publication/recovery matrix passed 343 crashes; `make lint test` passed all 280 commands. The next delivery integrates these verified components into foreground reads/writes and the public version-3 open/format path.

Foreground read evidence (2026-09-18): staged version-3 open/read integrates checkpoint loading, bounded replay, tail sanitation, cache lookup, and pressure publication. Four tests verify exact retry counters, zero/partial pages, failed-volume behavior, and version-1 rejection before writes. All 284 repository gate commands passed. Buffered writes and the public switch remain pending.

The ADR owns persistent encoding, durability, memory bounds, and compatibility. This plan owns delivery order and proof. Do not duplicate or quietly change the ADR's format contract in code.

## Agent instructions

Implement version-3 paged mapping and its bounded CLOCK cache in Rust. Read `README.md`, `AGENTS.md`, ADR-01, ADR-03, ADR-07, and the paging proposal in `papers/README.md` first. Rust is authoritative; leave Zig unchanged. Follow the repository's coding and technical-writing instructions.

Inspect the current worktree before editing. There are unrelated local changes at handoff, including `rust/src/lib.rs`, frontend/lab files, and `papers/README.md`. Do not reset, stage, or overwrite those changes. Request isolation or clarify overlapping edits before proceeding. Do not create commits, push, run privileged setup, or mutate devices without the user's authorization. The commit sequence below defines review units, not permission to commit.

Keep one serialized writer, fixed geometry, and a finite append log. Do not implement ADR-04 growth, segmentation, cleaning, discard, migration, generic caching infrastructure, or a metrics service. Do not suppress tests or weaken durability to make cache pressure pass.

Before code changes, inspect the actual framing, write buffering, FUA/flush paths, tail clearing, checkpoint selection, and accounting in the current checkout. Known starting points are `rust/src/lib.rs`, `rust/src/checkpoint.rs`, `rust/src/io_accounting.rs`, `rust/src/tests/`, and the ublk/lab binaries. These are navigation hints, not permission to assume the code still matches the older README.

## 1. Deliver the isolated cache first

First code review unit: `rust/src/mapping_cache.rs` and `rust/src/mapping_cache/clock.rs`, or equivalent names consistent with the crate. Add the minimal module declaration and tests, but do not replace the engine map yet.

Define naturally aligned mapping-entry arrays, fixed-capacity slots, page-ID lookup, dirty tracking, and a statistics snapshot. Keep CLOCK bookkeeping separate from slot ownership and cached values. Use concrete types and a small internal policy boundary; do not add trait objects or a public generic cache library merely for a hypothetical LRU implementation.

The cache has no backing file, I/O submission, checkpoint cursor, or persistence method. Its engine-facing outcomes distinguish a resident page, an available slot for a miss, and a need to checkpoint. Keep the acquisition/retry contract explicit so retries do not double-count misses. Do not expose a mutable entry without ensuring that mutation marks its page dirty.

Settle allocation accounting in this delivery. Document the hash-table implementation and its maximum allocated bytes at the selected capacity. Include spare buckets, policy bits, slot padding, and allocation capacities. If that cannot be bounded through the selected implementation, propose the smallest alternative before integrating it. Use integer arithmetic for the default `logical_bytes / 10_000` budget, checked calculations, whole-slot rounding, the one-slot minimum, and the page-count cap.

Dedicated unit tests run without `io_uring`, backing files, ublk, or root. Cover:

- Empty cache, one-slot cache, capacity rounding, minimum allocation, complete-map cap, and rejected invalid configuration.
- Insert, hit, miss, zero-index synthetic page acquisition, replacement, slot reuse, and lookup consistency after eviction.
- CLOCK second chances, cursor wrap, clean/dirty mixtures, all-dirty outcome, bounded scan count, and clean-state transition after simulated successful publication.
- Exact hit/miss, eviction, scan and pressure counters; separate foreground/recovery counters; no observations yielding an undefined numeric hit rate; no retry double counting.

Add layout assertions for `MappingEntries` size and alignment. Exercise a deterministic sequence against a simple reference model to check that lookup and slot ownership never diverge. Verify repeated churn does not grow allocations. Do not integrate a cache whose isolated tests or metrics are incomplete.

## 2. Deliver version-3 codecs and checkpoint storage

Split into small review units: shared record codecs and layout tests; then index/descriptor codecs and formatter behavior. Keep ordinary hand-written review units below 500 changed lines where practical; split tests with the behavior they verify. Do not switch the existing engine to a half-supported format.

Implement ADR-07's explicit little-endian offsets and checksum rules. Add mapping snapshots as kind 2 in the shared record chain. Generalize footer-only names where necessary without broad unrelated cleanup. Version 2 remains assigned to ADR-04 and must not be reused.

Prove with fixtures and boundary tests:

- Exact mapping-array offsets, record size, zero padding, last partial page, and physical/checksum zero-pair rules.
- Data records and standalone snapshots chained in both orders; consecutive LSNs; snapshot records with no payload; maximum data-record traversal bounds.
- Complete padded index-body checksums, fixed slot geometry, generation ordering, and conflicting equal generations.
- Invalid versions, kinds, identity, pointers, geometry, checksums, arithmetic overflow, truncation, and malformed lengths fail without panic or unintended writes.

Use a test-only codec/formatter path until full open/read/write/recovery support exists. Do not expose a default formatter that emits images the engine cannot open.

Before engine integration, review two correctness details explicitly and amend ADR-07 if needed: bounded multi-pass recovery ordering, and append-space reservation for foreground plus recovery mapping snapshots. Write the persistence sequence and demonstrate that a crash never overwrites the replay source or advances the root past unrepresented data. Stop for review if the proof requires changing the accepted scope.

## 3. Integrate lookup, persistence, and bounded recovery

Split this phase into dependent review units: normal lookup/write integration; checkpoint publication and reservation; then recovery and frontend activation. Intermediate units must remain testable without enabling an incomplete production path.

Replace dense per-LBA arrays with the resident disk-location index and cache. Reads validate fetched mapping snapshots before use. Missing zero-index pages are constructed as zero mappings without metadata I/O. Writes acquire required cache capacity before append, retain exact-completion publication semantics, and mark updated pages dirty. Avoid duplicate cached copies and unbounded request pinning.

The engine handles the cache's checkpoint-required outcome. Publication writes only dirty pages, streams the complete candidate index, and selects it only after descriptor persistence. Mark pages clean at that point, not after snapshot submission. An I/O failure follows the existing failed-volume policy. Reserve snapshot append space before accepting foreground writes; capacity errors must precede irreversible acceptance.

Preserve data flush/FUA guarantees without requiring a checkpoint. Check both engine and ublk behavior, including any current write buffer. A successful barrier leaves sufficient durable data-record metadata for replay; it need not clean the cache.

Recovery validates checkpoint candidates and their referenced pages with bounded buffers. It handles mixed record kinds, ignores snapshots as logical updates, and supports older-checkpoint fallback. Implement the ADR's bounded multi-pass fallback when replay exceeds cache capacity. Do not publish at an intermediate replay cursor or allocate a replay map proportional to all touched LBAs.

Recovery tests must cover a tail larger than the cache and fallback spanning multiple checkpoints, including a different smaller cache budget on reopen. Count repeated metadata and tail reads. Verify the fast path and bounded fallback produce identical mappings. Test no remaining append space and a crash during recovery snapshot production and final publication. Repeated recovery attempts must not reinterpret uncommitted snapshots as data writes.

Once the complete path passes focused tests, activate version 3 for new formats and explicitly reject older versions on open without writing. Preserve pre-change version-1 fixtures to prove rejection is non-mutating. Document that this is not an in-place upgrade.

Expose cache statistics through an engine snapshot and existing daemon/lab reporting. Report foreground hits, misses and derived hit rate separately from recovery; include page reads/writes, index bytes, pressure checkpoints, dirty/resident/capacity counts, cache bytes and index bytes. Do not use internal checkpoint acquisitions to improve the reported hit rate.

## 4. Prove the complete change and update status

Use only test-created disposable regular files by default. Live ublk/filesystem tests require operator permission and the existing identity-checking harness. No arbitrary backing device or privileged setup is authorized by this document.

Extend the existing fault-injection harness rather than building a separate simulator. Test interruption before and after mapping-page append completion, log fsync, index-body writes/fsync, descriptor writes/fsync, and recovery publication. Include corruption of a newest-checkpoint mapping page to force older-candidate fallback. Verify flushed data survives without a checkpoint, no dirty page is lost under pressure, and uncertain failures never resume writes.

Use tiny caches to force repeated eviction and a deterministic access sequence to establish exact hit-rate accounting. Test sequential, concentrated, and scattered accesses; reads and writes; moving working sets; and final partial pages. Verify that an unchanged mapping page keeps its physical pointer across checkpoints and a changed page gets a new one.

Measure default-budget allocation for 4 GiB, 400 GiB and 4 TiB geometries without allocating full-size backing files. Report requested/effective cache bytes, slots, lookup and policy bytes, resident index bytes, checkpoint peak allocation, and bounded recovery allocation. Measure process RSS separately on small executable fixtures. Record the conditions rather than inferring hit rate from mapping coverage.

Run focused Cargo tests during each delivery. Run the current repository `make lint test` gates before calling the implementation complete. Run `make test-acceptance` for the crash matrix. Run approved live regression gates for ublk and the accepted filesystem/SQLite paths; report skipped gates and why rather than claiming they passed.

Update `README.md` with format compatibility, cache configuration, statistics, memory accounting, and remaining finite-log limits. Mark ADR/plan implementation status only after execution evidence exists. Do not present microbenchmarks as sustainable-overwrite performance.

### Final agent handoff

Report changed paths, delivery units, exact commands and outcomes, format version, default/effective cache sizing, measured hit rate with its access sequence, and unresolved limitations. State whether bounded recovery and crash fallback were executed or remain unverified. Do not claim completion while allocation bounds, persistence ordering, or mandatory regression failures remain unresolved.
