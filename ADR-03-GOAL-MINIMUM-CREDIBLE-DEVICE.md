# ADR-03 Goal 2: minimum credible device

Date: 2026-08-29
Author: Miguel Filipe
Status: accepted
Goal status: active
On-disk format: 1
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-02](ADR-02-GOAL-1-BASIC-READ-WRITE.md)

## Context

V0.4 is a storage-engine library. It supports serialized 4 KiB reads and writes and survives a clean close, but it deliberately rejects an uncheckpointed log tail. A hard process exit after a successful write can therefore leave a volume that refuses to reopen.

V0.4 also has no ublk frontend. Nothing in the repository creates a Linux block device or serves kernel block requests.

The next goal is not a production device. It is the smallest device for which we can state and test a useful durability contract.

## Decision

We will complete four ordered deliveries:

1. **V0.5:** implement multiple-write and overwrite semantics.
2. **V0.6:** implement bounded crash recovery.
3. **V0.7:** expose the V0.6 engine through a serialized 4 KiB ublk frontend.
4. **V0.8:** validate the complete path with `fio`, graceful restarts, hard process exits and finite-log exhaustion.

[Goal 3: backing-medium fault resilience](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md#goal-3-backing-medium-fault-resilience) handles faults while the process remains alive. This goal requires correct recovery after fail-stop process loss and detection of persistent corruption under the ADR-01 fault model.

## What does durable mean?

The device contract is:

- A successful `WRITE` completion does not make that write durable.
- A successful `FLUSH` makes every earlier successful write durable.
- A clean close flushes the log. It publishes a checkpoint when `last_lsn > checkpoint_lsn`.
- After a crash, every write covered by a successful `FLUSH` must be visible.
- A write not covered by a successful `FLUSH` may be visible or absent.
- Any unflushed records that recovery exposes must form a valid prefix.
- Recovery must never return payload data whose checksum is invalid.

The contract assumes that a successful backing-file fsync provides the durability promised by Linux and the storage device.

Persistent bytes are untrusted input. Startup uses an older valid checkpoint when the newer descriptor or body is unusable, and returns a corruption error when neither root is usable. Tail recovery stops at the first invalid footer and clears the bounded stale-tail window. `read_block` returns an error when a payload checksum is invalid. Persistent input must not trigger an assertion or panic. Assertions remain valid for internal invariants after input validation.

## How do we model process loss?

All automated external process-loss cases initially use `SIGKILL` (`kill -9`). The test parent waits until the child reports a named persistence boundary and then kills it. This gives deterministic crash placement without running cleanup or destructors.

For storage recovery, this models:

- a user running `kill -9`;
- an out-of-memory killer terminating the process;
- abrupt service-manager termination.

It does not model a kernel panic, controller reset, hard reboot or power loss. Those failures can interrupt or reorder storage operations below the process. [Goal 3: backing-medium fault resilience](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md#goal-3-backing-medium-fault-resilience) models those effects with deterministic fault injection. A real power-cycle test can later provide additional evidence, but it is not an ADR-03 exit condition.

One separate child test will terminate through an uncaught Rust panic to model an internal assertion failure. The default Rust panic behavior can unwind and run destructors, so this case is not equivalent to `SIGKILL` and does not replace the `SIGKILL` matrix.

`SIGTERM` is not a crash. The ublk daemon must treat it as a graceful request: stop accepting work, drain requests, close the volume and remove the device.

## Preconditions

- The Git worktree is clean.
- `make lint` passes for Rust.
- `make test` runs and passes the complete Rust test suite.

No delivery work starts before these checks pass.

## Delivery 1: V0.5 update semantics

V0.5 uses the existing one-payload-per-footer encoding and does not change the format.

The block API has no delete operation. For this delivery, clearing an LBA means writing a 4 KiB zero block through `write_block`. Durable discard and physical-space reclamation remain deferred.

Required behavior:

- Write multiple distinct logical block addresses (LBAs).
- Overwrite one LBA more than once.
- Return the value from the latest completed write before flush.
- Return the latest durable value after flush, clean close and reopen.
- Clear an LBA by overwriting it with a 4 KiB zero block.
- Point both checkpoint maps at the latest physical payload and its checksum.
- Leave the visible mapping unchanged after an out-of-range or log-full write.

Acceptance tests:

- [x] Multiple LBAs read correctly before and after reopen.
  Evidence (2026-08-29): `cargo test multiple_lbas_read_correctly_before_and_after_reopen` passes with two distinct LBAs before and after a clean reopen.
- [x] Repeated overwrite of one LBA returns only the latest value.
  Evidence (2026-08-29): `cargo test repeated_overwrite_returns_only_latest_value` passes after three distinct non-zero writes to one LBA, reading each latest completed value, then explicitly flushing, cleanly closing, reopening through `open`, and reading only the third value.
- [x] Overwriting an LBA with zeroes returns zeroes before and after reopen.
  Evidence (2026-08-29): `cargo test overwriting_lba_with_zeroes_returns_zeroes_before_and_after_reopen` passes after a normal zero-block overwrite, both before and after a clean reopen.
- [x] Raw footer linkage and local sequence numbers are contiguous.
  Evidence (2026-08-29): `cargo test raw_footer_linkage_and_lsns_are_contiguous` passes after three public API writes. It verifies each footer's previous-footer link, self-position and contiguous local sequence numbers (LSNs) 1 through 3.
- [x] The clean checkpoint maps each LBA to its latest payload and checksum.
  Evidence (2026-08-29): `cargo test clean_checkpoint_maps_each_lba_to_latest_payload_and_checksum` passes after public writes and an overwrite. It inspects the raw clean-checkpoint body and verifies each LBA's latest physical payload and corresponding checksum.
- [x] An out-of-range write leaves mapping and cursors unchanged.
  Evidence (2026-08-29): `cargo test out_of_range_write_leaves_mapping_and_cursors_unchanged` passes through `Volume::write_block`, preserving the physical and checksum maps, all LSN/durability/footer/checkpoint-byte cursors, checkpoint slot, failed state and prior readable value.
- [x] A log-full write leaves the last successful value readable after reopen.
  Evidence (2026-08-29): `cargo test log_full_write_leaves_last_successful_value_readable_after_reopen` passes with the one-record backing profile, proving the rejected second write leaves the physical and checksum maps, all LSNs, footer position, raw log bytes, checkpoint-byte cursor, checkpoint slot and failed state unchanged, while preserving the first value before and after clean close/reopen.
- [x] All work is git committed with sensible commit messages.
  Evidence (2026-08-29): at evidence capture, `git status --short` was empty. `git log 996c6bf..e61deed` shows nine focused Delivery 1 test commits with descriptive messages.
- [x] All new tests and code run through the top-level `make test` target.
  Evidence (2026-08-29): `make test` delegates to `test-rust` and `cargo test`, running all 22 Rust tests with 22 passed and none ignored.
- [x] `make lint` and `make test` pass.
  Evidence (2026-08-29): top-level `make lint` and `make test` both pass; the test result is 22 passed, 0 failed, 0 ignored.

## Delivery 2: V0.6 crash recovery

Recovery starts from the newest usable checkpoint and scans records in local sequence number (LSN) order.

For each candidate footer, recovery validates:

- magic, format version and record kind;
- footer checksum and zero padding;
- volume identity and expected local sequence number;
- previous-footer link and expected footer position;
- payload count and backing-store bounds;
- LBA range and uniqueness;
- zeroes in unused LBA and checksum entries.

Recovery applies a record to the in-memory map only after its complete footer is valid. Recovery defers payload validation until `read_block`, which verifies the checksum before returning bytes.

At the first missing or invalid record, recovery ignores everything after the gap. In on-disk format version 1, `maximum_record_blocks` is 339: 338 payload blocks plus one footer. Recovery clears and flushes `min(339, backing_blocks - append_block)` blocks at the recovered append position so stale records cannot reappear without writing past the backing store.

Recovery reconstructs all cursors before serving requests:

- `last_lsn` and `last_footer_block` identify the final replayed record;
- `durable_lsn = last_lsn` after the stale-tail flush;
- `checkpoint_lsn` remains the selected checkpoint LSN;
- `log_bytes_since_checkpoint` equals the replayed physical bytes.

Recovery is bounded by `checkpoint_after_bytes`. `open` uses a 64 MiB default, while an options-based open call lets tests and the daemon override it. The value is a 4 KiB-aligned physical-byte count and must be at least `339 × 4 KiB`. Before accepting a record that would exceed the bound, the engine checkpoints first.

Interrupted checkpoint publication is part of V0.6. If the newest descriptor or body is unusable, open must use an older valid checkpoint and replay its tail. This behavior cannot remain deferred while we claim recovery from process loss at any persistence boundary.

Crash placement uses only boundaries the child can report after an exact I/O completion. [Goal 3: backing-medium fault resilience](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md#goal-3-backing-medium-fault-resilience) injects interruption inside a backing operation.

Required crash boundaries:

- record write completion, before mapping publication;
- mapping publication, before flush submission;
- backing fsync completion, before `durable_lsn` advances;
- each completed checkpoint-body block write;
- checkpoint-body fsync completion, before descriptor submission;
- descriptor write completion, before descriptor fsync;
- descriptor fsync completion, before in-memory checkpoint publication;
- each completed stale-tail clear write;
- stale-tail fsync completion, before serving requests.

Acceptance tests:

- [x] Recover one flushed write after `SIGKILL` without close.
  Evidence (2026-08-29): `cargo test --quiet --lib tests::recover_one_flushed_write_after_sigkill_without_close -- --exact` passes 20 consecutive runs; the parent bounds the child's post-flush handshake wait to 10 seconds, kills and reaps only that child, verifies `SIGKILL`, and reads the block through public `open` without `Volume::close`.
- [x] Recover multiple flushed writes and overwrites after `SIGKILL`.
  Evidence (2026-08-29): `cargo test --quiet --lib tests::recover_multiple_flushed_writes_and_overwrites_after_sigkill -- --exact` passes 20 consecutive runs after one flush covering six contiguous records; the parent then kills and reaps only its child, verifies `SIGKILL`, and public `open` recovers three distinct LBAs including two latest overwrite values.
- [x] Preserve every write covered by the last successful flush.
  Evidence (2026-08-29): the same 20-run crash test writes all three LBAs and three overwrites before its single successful flush, reports the post-flush boundary, and after parent-driven `SIGKILL` verifies every covered LBA and latest value through public `open` and `read_block`.
- [x] Accept either the old or new state for writes not covered by flush.
  Evidence (2026-08-29): `recover_after_kill_at_record_write_boundary` and `recover_after_kill_at_mapping_publication_boundary` each pass 20 consecutive runs after starting from a flushed old value; public `open` and `read_block` accept only the complete old or new value after the unflushed overwrite is killed.
- [x] Recover after a kill at the record-write boundary.
  Evidence (2026-08-29): `cargo test --quiet --lib tests::recover_after_kill_at_record_write_boundary -- --exact` passes 20 consecutive runs with `SIGKILL` after exact record-write completion and before mapping publication.
- [x] Recover after a kill at the mapping-publication boundary.
  Evidence (2026-08-29): `cargo test --quiet --lib tests::recover_after_kill_at_mapping_publication_boundary -- --exact` passes 20 consecutive runs with `SIGKILL` after mapping publication and before flush.
- [x] Recover after a kill at the log-fsync boundary.
  Evidence (2026-08-29): `cargo test --quiet --lib tests::recover_after_kill_at_log_fsync_boundary -- --exact` passes 20 consecutive runs with `SIGKILL` after backing fsync completion and before durable-cursor advance; recovery requires the flushed new value.
- [x] Recover after each checkpoint-body block boundary.
  Evidence (2026-08-29): `recover_after_each_checkpoint_body_block_boundary` passes 20 parent-driven `SIGKILL` runs at each of five completed blocks in a multi-block checkpoint body; public `open` falls back to the older root, replays the flushed tail and returns every latest write and overwrite.
- [x] Recover after the checkpoint-body fsync boundary.
  Evidence (2026-08-29): `recover_after_checkpoint_body_fsync_boundary` passes 20 parent-driven `SIGKILL` runs after body fsync and before descriptor submission; public `open` proves older-root fallback plus replay of every flushed latest value.
- [x] Recover after the descriptor-write boundary.
  Evidence (2026-08-29): `recover_after_descriptor_write_boundary` passes 20 parent-driven `SIGKILL` runs after exact descriptor-write completion and before descriptor fsync; public `open` selects the complete newer root and returns every flushed latest value.
- [x] Recover after the descriptor-fsync boundary.
  Evidence (2026-08-29): `recover_after_descriptor_fsync_boundary` passes 20 parent-driven `SIGKILL` runs after descriptor fsync and before in-memory publication; public `open` selects the durable newer root and returns every flushed latest value.
- [x] Recover after each stale-tail clearing boundary.
  Evidence (2026-08-29): `recover_after_each_stale_tail_clear_block_boundary` passes 20 parent-driven `SIGKILL` runs after each of all 339 completed clear writes, and `recover_after_stale_tail_fsync_boundary` passes 20 runs after the final clear fsync. Each recovery preserves the valid prefix and completes the full bounded clear before opening.
- [x] Fall back independently from a deliberately corrupted newest descriptor and checkpoint body.
  Evidence (2026-08-29): `cargo test --lib falls_back_from_corrupted_newest` passes two independent public-`open` cases. Each starts with two checksum-valid roots plus a flushed replay tail, flips one deterministic raw byte in only the newest descriptor or only its checkpoint body, explicitly rejects a panic, selects the older checkpoint LSN, replays through the latest LSN and returns every latest flushed value.
- [x] Stop at an invalid tail and never resurrect a valid-looking later record.
  Evidence (2026-08-29): `invalid_gap_stops_replay_and_clears_only_bounded_window` writes a valid record, an invalid gap and a checksummed valid-looking later record; public recovery stops at LSN 1, returns zeroes for the later LBA and leaves the ignored later record outside the clear window intact.
- [x] Clear the complete bounded stale-tail window durably before serving requests.
  Evidence (2026-08-29): `recovery_accepts_every_format_one_payload_count` replays valid format-version-1 records containing every payload count from 1 through 338. `invalid_gap_stops_replay_and_clears_only_bounded_window` verifies all 339 stale blocks are zero while a valid-looking later record remains ignored and intact. `stale_tail_clear_stops_at_backing_eof` verifies the clear is bounded to a 17-block remainder without extending the file, and `recover_after_stale_tail_fsync_boundary` verifies the full window is durable after 20 parent-driven `SIGKILL` runs at the final fsync.
- [x] Reconstruct every recovery cursor from replayed state.
  Evidence (2026-08-29): `cargo test --lib tests::reconstructs_every_recovery_cursor_from_replayed_state -- --exact` passes through public `open` from a valid older checkpoint plus three replayed records, including a three-payload format-version-1 record. It verifies exact final LSN, footer, durability, selected-checkpoint, physical-byte and next-slot cursors, append-position derivation, latest physical/checksum mappings and readable values.
- [x] Reject two unusable checkpoint roots with a corruption error, not a panic.
  Evidence (2026-08-29): `cargo test --lib tests::rejects_two_unusable_checkpoint_roots_without_panic_or_mutation -- --exact` corrupts the newest descriptor and the older checkpoint body through synced raw bytes. Two public `open` attempts inside `catch_unwind` return `InvalidData` without serving the image, panicking or changing any backing bytes.
- [x] Reject invalid footer ranges, duplicate LBAs and non-zero unused entries.
  Evidence (2026-08-29): `cargo test rejects_invalid_footer_` passes seven fresh-image cases through public `open`, using checksum-valid raw footers for zero and 339 payloads, a declared footer position beyond backing EOF, an out-of-range LBA, duplicate LBAs, and non-zero unused LBA and checksum entries. Each case stops at the prior valid record, publishes no invalid mapping, does not panic or read out of bounds, clears and flushes exactly the backing-bounded stale-tail window, and preserves the first block beyond that window when present.
- [ ] Checkpoint before accepting a record that would exceed the replay bound.
- [ ] Reject invalid `checkpoint_after_bytes` values.
- [ ] Recover after one uncaught unwinding Rust panic in a child process.
- [ ] All new tests and code pass through the top-level `make lint test` targets.

## How do we test finite logs?

Tests size backing stores by record capacity rather than arbitrary byte sizes. A V0 write consumes two 4 KiB physical blocks: one payload and one footer.

Use two profiles:

- **one-record:** `backing_blocks = log_start + 2`. The first write succeeds and the second reports log full.
- **thirty-two-record:** `backing_blocks = log_start + 64`. This supports multiple writes, overwrites, crash recovery and a predictable final log-full boundary.

Both profiles must prove:

- every successful write remains readable;
- the first write beyond capacity returns the expected error;
- the failed write does not advance mapping or log cursors;
- flush, close and reopen still work after exhaustion.

Regular files provide exact sizes and are the required automated medium. Logical Volume Manager (LVM) logical volumes allocate in larger extents and are used for the later V0.8 vertical validation, not the exact one-record geometry test.

## Delivery 3: V0.7 smallest ublk frontend

The frontend is deliberately serialized:

- one hardware queue;
- queue depth one;
- 4096-byte logical and physical block size;
- maximum request size of 4096 bytes;
- READ, WRITE and FLUSH only;
- volatile write-cache semantics;
- no advertised Force Unit Access (FUA), discard or write-zeroes support.

ublk addresses data in 512-byte sectors even when the logical block size is 4096 bytes. READ and WRITE accept a request only when:

- `nr_sectors == 8`;
- `start_sector % 8 == 0`;
- `start_sector <= dev_sectors && 8 <= dev_sectors - start_sector`.

The frontend maps `start_sector / 8` to the engine LBA. The 4096-byte maximum lets the kernel split larger requests, so this delivery needs no batching or multi-block engine API.

The frontend does not advertise FUA. If an unexpected request carries the FUA flag, it returns `EOPNOTSUPP` rather than silently weakening durability.

The frontend must:

- expose the engine's volume size;
- hold an exclusive lock on the backing store;
- copy request data through aligned engine buffers;
- map engine errors to negative errno results;
- return the completed byte count for READ and WRITE;
- call `Volume::flush` for FLUSH;
- complete the request that observes a fatal engine error with its mapped negative errno;
- reject queued and subsequent requests without issuing more engine I/O after the volume enters its failed state;
- drain requests and call `Volume::close` on graceful shutdown;
- delete the ublk device when shutdown completes.

Acceptance tests:

- [ ] A device appears with the expected 4 KiB geometry and size.
- [ ] Aligned READ and WRITE requests reach the expected engine LBA.
- [ ] FLUSH uses the engine durability path.
- [ ] Unsupported or invalid requests return an error without panic or hang.
- [ ] A second daemon cannot open the same backing store.
- [ ] `SIGTERM` performs a clean close and device removal.
- [ ] READ and WRITE reject invalid length, alignment, range and flags.
- [ ] `SIGKILL` leaves storage recoverable by a new daemon.
- [ ] All new tests and code pass through the top-level `make lint test` targets.

ADR-03 does not enable transparent ublk user recovery (`UBLK_F_USER_RECOVERY`). After `SIGKILL`, the harness waits for the old device to disappear or deletes its recorded device ID through the ublk control interface. It then creates a new device and starts a new `fio` verification process. The device ID may change, and any request that was in flight at the kill may fail.

## Delivery 4: V0.8 vertical functional validation

The required environment uses a disposable regular file. A dedicated LVM logical volume is optional operator-run evidence. Tests must never target the laptop's system NVMe, mounted filesystems or an unnamed block device.

Each scenario and each repetition starts from a newly formatted backing file. Successful scenarios use at most 16 writes on the thirty-two-record profile. The exhaustion scenario alone attempts 33 writes and expects the final write to fail.

Required `fio` scenarios use direct 4 KiB I/O through `/dev/ublkbN`:

- sequential write, flush, read and verify;
- random write, flush, read and verify;
- repeated overwrite and verify;
- graceful daemon restart and verify;
- deterministic `SIGKILL` after a successful `fio` flush, restart and verify;
- deterministic `SIGKILL` after a named checkpoint-body or descriptor boundary, restart and verify;
- finite-log exhaustion with the thirty-two-record profile, followed by restart and verification of the 32 successful writes.

`fio` must verify its own data pattern in addition to the engine's XXH3-64 checksums. The test harness sets a timeout for every daemon and `fio` process and cleans up only the ublk device ID and backing file that it created.

Acceptance evidence:

- [ ] A repository script creates, runs and cleans up the regular-file ublk test with timeouts.
- [ ] Every regular-file `fio` scenario passes three consecutive fresh-image runs.
- [ ] No scenario hangs after a daemon error or exit.
- [ ] ublk reports the 33rd write as `ENOSPC`.
- [ ] The daemon restarts cleanly after every hard-exit scenario.
- [ ] All new tests and code pass through the top-level `make lint test` targets.

Creating or formatting an ext4 or XFS filesystem is not part of this goal.

## Operator safety

The required regular-file harness creates its own file in a temporary directory and refuses a caller-supplied backing path. It records the ublk device ID returned by the daemon and verifies `/sys/block/ublkbN` before invoking `fio`. Cleanup acts only on those recorded resources.

An optional dedicated logical-volume run requires a separate explicit flag and a path whose logical-volume name starts with `my-block-storage-test-`. Before writing, the operator procedure verifies that the path:

1. is an LVM logical volume;
2. is not mounted and has no mounted child;
3. has no filesystem or RAID signature;
4. is not the source, parent or holder of the system root, boot, swap or home device.

The procedure aborts without writing if a check fails or returns an ambiguous result. Ralph never runs this optional procedure.

## Goal exit

ADR-03 is complete only when:

- [x] ADR-02 is closed.
- [ ] Every V0.5, V0.6, V0.7 and V0.8 acceptance item is checked.
- [ ] The full Rust gate passes without skipped or ignored tests.
- [ ] Every V0.6 automated crash-boundary scenario passes twenty consecutive runs.
- [ ] Every V0.8 regular-file scenario passes three consecutive fresh-image runs.
- [ ] The regular-file harness validates every resource it creates before writing.
- [ ] Test evidence records the commit, kernel, backing type, commands and results.
- [ ] All new tests and code pass through the top-level `make lint test` targets.

## Consequences

The device remains finite and slow. A filesystem or long `fio` workload can fill the append-only log. We accept this because predictable exhaustion is safer than adding compaction before recovery is proven.

`SIGKILL` gives deterministic evidence for fail-stop process recovery. It does not prove behavior under torn or reordered medium writes. ADR-01 places that work immediately after this goal.
