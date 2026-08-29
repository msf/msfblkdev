# ADR-03 Goal: minimum credible device

Date: 2026-08-29
Author: Miguel Filipe
Status: accepted, active
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-02](ADR-02-GOAL-1-BASIC-READ-WRITE.md)

## Context

V0.4 is a storage-engine library. It reads and writes one 4 KiB block and survives a clean close, but it deliberately rejects an uncheckpointed log tail. A hard process exit after a successful write can therefore leave a volume that refuses to reopen.

V0.4 also has no ublk frontend. Nothing in the repository creates a Linux block device or serves kernel block requests.

The next goal is not a production device. It is the smallest device for which we can state and test a useful durability contract.

## Decision

We will complete four ordered deliveries:

1. Implement V0.5 multiple-write and overwrite semantics.
2. Implement V0.6 bounded crash recovery.
3. Expose the V0.6 engine through a serialized 4 KiB ublk frontend.
4. Validate the complete path with fio, graceful restarts, hard process exits and finite-log exhaustion.

A later goal handles backing-medium faults while the process remains alive. This goal only requires correct recovery after fail-stop process loss and correct detection of persistent corruption already covered by the format.

## What does durable mean?

The device contract is:

- A successful `WRITE` completion does not make that write durable.
- A successful `FLUSH` makes every earlier successful write durable.
- A clean close performs the required flush and checkpoint publication.
- After a crash, every write covered by a successful `FLUSH` must be visible.
- A write not covered by a successful `FLUSH` may be visible or absent.
- Recovery may expose only a valid prefix of unflushed records.
- Recovery must never return payload data whose checksum is invalid.

The contract assumes that a successful backing-file fsync provides the durability promised by Linux and the storage device.

Persistent bytes are untrusted input. Corrupt descriptors, footers, maps or payloads must return an error or use a valid older root. They must not trigger an assertion or panic. Assertions remain valid for internal invariants that cannot be caused by persistent input after validation.

## How do we model process loss?

All automated external process-loss cases initially use `SIGKILL` (`kill -9`). The test parent waits until the child reports a named persistence boundary and then kills it. This gives deterministic crash placement without running cleanup or destructors.

For storage recovery, this models:

- a user running `kill -9`;
- an out-of-memory killer terminating the process;
- abrupt service-manager termination.

It does not model a kernel panic, controller reset, hard reboot or power loss. Those failures can interrupt or reorder storage operations below the process. Goal 3 models those effects with deterministic backing-medium fault injection. A real power-cycle test can later provide additional evidence, but it is not an ADR-03 exit condition.

One separate child test will terminate through an uncaught Rust panic to model an internal assertion failure. We do not repeat the complete crash matrix for each equivalent process-termination mechanism.

`SIGTERM` is not a crash. The ublk daemon must treat it as a graceful request: stop accepting work, drain requests, close the volume and remove the device.

## Preconditions

- The Git worktree is clean.
- `make lint` passes for Rust.
- `make test` runs and passes the complete Rust test suite.

No Delivery 1 work starts before these checks pass.

## Delivery 1: V0.5 update semantics

V0.5 uses the existing one-payload-per-footer format. No format change is expected.

The block API has no delete operation. For this delivery, clearing an LBA means writing a 4 KiB zero block through `write_block`. Durable discard and physical-space reclamation remain deferred.

Required behavior:

- Write multiple distinct logical block addresses (LBAs).
- Overwrite one LBA more than once.
- Return the latest completed value before flush.
- Return the latest durable value after flush, clean close and reopen.
- Clear an LBA by overwriting it with a 4 KiB zero block.
- Point the checkpoint map at the latest physical payload.
- Leave the visible mapping unchanged after an invalid or log-full write.

Acceptance tests:

- [ ] Multiple LBAs read correctly before and after reopen.
- [ ] Repeated overwrite of one LBA returns only the latest value.
- [ ] Overwriting an LBA with zeroes returns zeroes before and after reopen.
- [ ] Raw footer linkage and local sequence numbers are contiguous.
- [ ] The clean checkpoint maps each LBA to its latest payload and checksum.
- [ ] An out-of-range write leaves mapping and cursors unchanged.
- [ ] A log-full write leaves the last successful value readable after reopen.
- [ ] All work is git committed with sensible commit messages.
- [ ] All new tests and code run through the top-level `make test` target.
- [ ] `make lint` and `make test` pass.

## Delivery 2: V0.6 crash recovery

Recovery starts from the newest usable checkpoint and scans records in local sequence number order.

For each candidate footer, recovery validates:

- magic, format version and record kind;
- footer checksum and zero padding;
- volume identity and expected local sequence number;
- previous-footer link and expected footer position;
- payload count and backing-store bounds;
- LBA range and uniqueness;
- zeroes in unused LBA and checksum entries.

Recovery applies a record to the in-memory map only after its complete footer is valid. Payload corruption remains lazy: `read_block` detects it before returning bytes.

At the first missing or invalid record, recovery ignores everything after the gap. It clears and flushes `min(maximum_record_blocks, backing_blocks - append_block)` blocks at the recovered append position so stale records cannot reappear without writing past the backing store.

Recovery reconstructs all cursors before serving requests:

- `last_lsn` and `last_footer_block` identify the final replayed record;
- `durable_lsn = last_lsn` after the stale-tail flush;
- `checkpoint_lsn` remains the selected checkpoint LSN;
- `log_bytes_since_checkpoint` equals the replayed physical bytes.

Recovery is bounded by `checkpoint_after_bytes`. `open` uses a 64 MiB default, while an options-based open call lets tests and the daemon override it. The value is a 4 KiB-aligned physical-byte count and must be at least one maximum-sized record (339 blocks). Before accepting a record that would exceed the bound, the engine checkpoints first.

Interrupted checkpoint publication is part of V0.6. If the newest descriptor or body is unusable, open must use an older valid checkpoint and replay its tail. This behavior cannot remain deferred while we claim recovery from process loss at any persistence boundary.

Crash placement uses only boundaries the child can report after an exact I/O completion. Goal 3 injects interruption inside a backing operation.

Required crash boundaries:

- record write completion, before mapping publication;
- mapping publication, before flush submission;
- backing fsync completion, before the durable cursor advances;
- each completed checkpoint-body block write;
- checkpoint-body fsync completion, before descriptor submission;
- descriptor write completion, before descriptor fsync;
- descriptor fsync completion, before in-memory checkpoint publication;
- each completed stale-tail clear write;
- stale-tail fsync completion, before serving requests.

Acceptance tests:

- [ ] Recover one flushed write after `SIGKILL` without close.
- [ ] Recover multiple flushed writes and overwrites after `SIGKILL`.
- [ ] Preserve every write covered by the last successful flush.
- [ ] Accept either the old or new state for writes not covered by flush.
- [ ] Recover after a kill at the record-write boundary.
- [ ] Recover after a kill at the mapping-publication boundary.
- [ ] Recover after a kill at the log-fsync boundary.
- [ ] Recover after each checkpoint-body block boundary.
- [ ] Recover after the checkpoint-body fsync boundary.
- [ ] Recover after the descriptor-write boundary.
- [ ] Recover after the descriptor-fsync boundary.
- [ ] Recover after each stale-tail clearing boundary.
- [ ] Fall back from an unusable newest descriptor or checkpoint body.
- [ ] Stop at an invalid tail and never resurrect a valid-looking later record.
- [ ] Clear the bounded stale-tail window durably before serving requests.
- [ ] Reconstruct every recovery cursor from replayed state.
- [ ] Reject two unusable checkpoint roots with a corruption error, not a panic.
- [ ] Reject invalid footer ranges, duplicate LBAs and non-zero unused entries.
- [ ] Checkpoint before accepting a record that would exceed the replay bound.
- [ ] Reject invalid `checkpoint_after_bytes` values.
- [ ] Recover after one uncaught panic in a child process.

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

Regular files provide exact sizes and are the required automated medium. Logical volume manager (LVM) logical volumes allocate in larger extents and are used for the later vertical durability run, not the exact one-record geometry test.

## Delivery 3: smallest ublk frontend

The frontend is deliberately serialized:

- one hardware queue;
- queue depth one;
- 4096-byte logical and physical block size;
- maximum request size of 4096 bytes;
- READ, WRITE and FLUSH only;
- volatile write-cache semantics;
- no advertised Force Unit Access (FUA), discard or write-zeroes support.

ublk addresses data in 512-byte sectors even when the logical block size is 4096 bytes. READ and WRITE require `nr_sectors == 8`, `start_sector % 8 == 0`, and the overflow-safe range `start_sector <= dev_sectors && 8 <= dev_sectors - start_sector`. The frontend maps `start_sector / 8` to the engine LBA. The 4096-byte maximum lets the kernel split larger requests, so this delivery needs no batching or multi-block engine API.

The frontend does not advertise FUA. If an unexpected request carries the FUA flag, it returns `EOPNOTSUPP` rather than silently weakening durability.

The frontend must:

- expose the engine's volume size;
- hold an exclusive lock on the backing store;
- copy request data through aligned engine buffers;
- map engine errors to negative errno results;
- return the completed byte count for READ and WRITE;
- call `Volume::flush` for FLUSH;
- stop and fail requests after the volume enters its failed state;
- drain and checkpoint on graceful shutdown;
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

ADR-03 does not implement transparent ublk user recovery. After `SIGKILL`, the test waits for the old device to disappear or deletes that recorded device ID through the ublk control interface. It then creates a new device and starts a new fio verification process. The device ID may change, and any request that was in flight at the kill may fail.

## Delivery 4: vertical functional validation

The required environment uses a disposable regular file. A dedicated LVM logical volume is optional operator-run evidence. Tests must never target the laptop's system NVMe, mounted filesystems or an unnamed block device.

Each scenario and each repetition starts from a newly formatted backing file. Successful scenarios use at most 16 writes on the thirty-two-record profile. The exhaustion scenario alone attempts 33 writes and expects the final write to fail.

Required fio scenarios use direct 4 KiB I/O through `/dev/ublkbN`:

- sequential write, flush, read and verify;
- random write, flush, read and verify;
- repeated overwrite and verify;
- graceful daemon restart and verify;
- deterministic `SIGKILL` after a successful fio flush, restart and verify;
- deterministic `SIGKILL` after a named checkpoint-body or descriptor boundary, restart and verify;
- finite-log exhaustion with the thirty-two-record profile, followed by restart and verification of the 32 successful writes.

fio must verify its own data pattern in addition to the engine's XXH3 checksums. The test harness sets a timeout for every daemon and fio process and cleans up only the ublk device ID and backing file that it created.

Acceptance evidence:

- [ ] A repository script creates, runs and cleans up the regular-file ublk test with timeouts.
- [ ] Every regular-file fio scenario passes three consecutive fresh-image runs.
- [ ] No scenario hangs after a daemon error or exit.
- [ ] ublk reports the 33rd write as `ENOSPC`.
- [ ] The daemon restarts cleanly after every hard-exit scenario.

Creating or formatting an ext4 or XFS filesystem is not part of this goal.

## Operator safety

The required regular-file harness creates its own file in a temporary directory and refuses a caller-supplied backing path. It records the ublk device ID returned by the daemon and verifies `/sys/block/ublkbN` before invoking fio. Cleanup acts only on those recorded resources.

An optional dedicated-LV run requires a separate explicit flag and a path whose logical-volume name starts with `my-block-storage-test-`. Before writing, the operator procedure verifies that the path:

1. is an LVM logical volume;
2. is not mounted and has no mounted child;
3. has no filesystem or RAID signature;
4. is not the source, parent or holder of the system root, boot, swap or home device.

Any failed or ambiguous check aborts without writing. Ralph never runs this optional procedure.

## Goal exit

ADR-03 is complete only when:

- [x] ADR-02 is closed.
- [ ] Every V0.5, V0.6, ublk and vertical acceptance item is checked.
- [ ] The full Rust gate passes without skipped or ignored tests.
- [ ] Every automated crash scenario passes twenty consecutive runs.
- [ ] The regular-file harness validates every resource it creates before writing.
- [ ] Test evidence records the commit, kernel, backing type, commands and results.

## Consequences

The device remains finite and slow. A filesystem or long fio workload can fill the append-only log. We accept this because predictable exhaustion is safer than adding compaction before recovery is proven.

`SIGKILL` gives deterministic evidence for fail-stop process recovery. It does not prove behavior under torn or reordered medium writes. ADR-01 places that work immediately after this goal.
