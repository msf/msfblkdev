# SQLite/ext4 performance spike

Status: proposed work-plan, revision 2, 2026-09-13

Revision 2 replaces the earlier ordering. The earlier plan treated request batching as one of
four parallel experiments. It is now the whole spike. The device currently uses the ublk API in
its slowest legal configuration, and the engine writes 2 bytes for every 1 byte of payload.
Fix those two things, measure each one, and stop.

Pause ADR-06 delivery work. Keep filesystem and database durability settings unchanged.

## What the code does today

Read from source, not measured:

| Fact | Location |
|---|---|
| One queue, queue depth 1. One request in flight for the life of the device. | `block-storage-ublk.rs`:36-37 |
| `max_sectors` is 8, so the maximum request is 4 KiB. | `block-storage-ublk.rs`:319 |
| `IO_BUFFER_BYTES` is one constant serving three jobs: logical block size, maximum request size, and device capacity arithmetic. | `block-storage-ublk.rs`:33,310,319 |
| The device advertises `UBLK_ATTR_VOLATILE_CACHE`, so the kernel already sends FLUSH where it needs durability. The engine does not exploit this. | `block-storage-ublk.rs`:317 |
| Each `write_block` builds one 8 KiB record (4 KiB payload + 4 KiB footer) and issues one `writev` plus one `submit_and_wait`. Two syscalls and 2x bytes per 4 KiB write. | `lib.rs`:203-275 |
| `write_block` zero-initializes an 8 KiB stack record, then overwrites 4 KiB of it with a copy of the caller's buffer. `read_block` zero-initializes a 4 KiB stack block before reading into it. | `lib.rs`:177,230-232 |
| The record decoder accepts up to 338 payloads under one footer. No writer has ever produced more than one. | `lib.rs` record format; recovery path |
| `flush()` is a bare `fsync`. FUA is rejected as unsupported. | `lib.rs`:137-151, `block-storage-ublk.rs`:1290 |
| Engine checkpoint interval is 64 MiB of appended log bytes. The checkpoint body writes 384 KiB in 96 separate submit-and-wait operations. | `checkpoint.rs`:17,50-167 |

## The three changes

Implement in this order. Each one is measured separately against the previous state.

### Change A: make the request path wide

Split `IO_BUFFER_BYTES` into three named constants before touching any value:

- `LOGICAL_BLOCK_BYTES` (4096): stays the logical sector size and the engine's block size.
- `MAX_REQUEST_BYTES` (new, 131072): sets `max_sectors` and the per-tag buffer size.
- Capacity arithmetic uses `LOGICAL_BLOCK_BYTES`, never the request size.

Then allocate one `IoBuf` per tag. The queue callback currently indexes `buffers[0]`; it must
index by tag.

#### Configuration: two knobs, one derived value

ublk fixes the per-tag buffer size and the queue depth when the device is added, so both are
startup configuration. Keep the surface small:

| Knob | Default | Constraint |
|---|---|---|
| `--max-request-bytes` | 131072 (128 KiB, 32 blocks) | multiple of `LOGICAL_BLOCK_BYTES`, page-aligned, at most 338 blocks |
| `--inflight-memory-bytes` | 4194304 (4 MiB) | at least one request |

Queue depth is **derived**, not configured: `depth = inflight_memory_bytes / max_request_bytes`,
clamped to 1 through 4096. At the defaults that is depth 32. Log the derived depth and the total
buffer memory at startup.

`QUEUE_COUNT` stays 1. It is a design invariant, not a tuning knob. The engine has one serialized
writer and one write-back buffer, so a second queue would add a second thread contending for both.
Do not derive queue count from the memory budget: that turns a sizing number into a concurrency
change.

The 338-block ceiling on the request size keeps the invariant that **one ublk request always fits
in one log record**. Assert it at startup. Do not derive the request size from the footer
capacity: 338 blocks is 1,384,448 bytes, which is not a power of two, and deriving it would tie
device geometry to the on-disk format.

`io_buf_bytes` and `basic.max_sectors` are separate ublk settings. Set `io_buf_bytes` to
`MAX_REQUEST_BYTES` and `max_sectors` to `MAX_REQUEST_BYTES >> 9`, never above it, because the
per-tag buffer has to hold the whole request.

ublk's own limits, read from `/usr/include/linux/ublk_cmd.h` and `libublk-0.4.7/src/ctrl.rs`:

| Limit | Value | Source |
|---|---|---|
| Per-I/O buffer size | 32 MiB maximum, page-aligned | `ublk_cmd.h`:144 (`UBLK_IO_BUF_BITS` 25); `ctrl.rs`:20,2181 (`MAX_BUF_SZ`) |
| libublk default buffer size | 512 KiB | `ctrl.rs`:777 |
| Queue depth | 4096 maximum | `ublk_cmd.h`:140 |

These caps are far above the values here. The real constraint is the memory budget, which is why
it is the knob.

A request larger than 4 KiB covers multiple consecutive logical blocks. The request handler
splits it into `LOGICAL_BLOCK_BYTES` units and hands them to the engine. Reject a request whose
start or length is not block-aligned, as today.

Expected effect: ext4 issues one request where it used to issue up to 32, and the kernel can keep
several requests outstanding. No change to bytes written.

Trap: the lab's sectors-divided-by-eight metric becomes a count of 4 KiB units, not a count of
requests. Record actual request counts and sizes instead.

### Change B: buffer writes in memory and release them on FLUSH or FUA

This is the change that matters. It is legal because the device advertises a volatile write
cache. Under that contract the device may complete a WRITE before the data is durable, and the
filesystem asks for durability with FLUSH and FUA. FUA is not a barrier. Linux removed write
barriers in 2010. `REQ_PREFLUSH` means flush the cache before this write, and `REQ_FUA` means this
write must be durable when it completes. Ordering is the filesystem's job, through completion
ordering, not the device's.

The new write path:

1. A WRITE copies its payload into a preallocated write-back buffer, indexed by logical block
   address, and completes immediately.
2. A second write to the same logical block address overwrites the buffered copy in place. Only
   the last version reaches the log. This also satisfies the decoder, which rejects duplicate
   addresses inside one record.
3. A READ checks the write-back buffer before the mapping table.
4. FLUSH, a write carrying FUA, a full buffer, or 338 buffered payloads triggers a release: the
   engine emits one record holding every buffered payload plus one footer, as one `writev`, then
   one `fsync`, then completes the pending request.

The record format does not change. Recovery already accepts multi-payload records.

Byte arithmetic, calculated not measured: footer overhead falls from 2.000x of payload bytes to
1.125x at 8 payloads per record, 1.031x at 32, and 1.003x at 338. Syscalls fall from two per
write to two per release.

Two hard requirements, both of which need their own tests:

- **Read-after-write must stay correct.** A read of a buffered, not-yet-logged block must return
  the buffered data. Test it with an overwrite followed immediately by a read, with no
  intervening flush.
- **`ENOSPC` must stay early.** Today write 33 into a 32-record volume fails at that write. If
  the buffer defers allocation, it would fail at the next flush instead, and ublk/fio acceptance
  scenario 7 would change meaning. Reserve the log blocks when the payload enters the buffer.
  Log allocation is deterministic, so this costs nothing. Return `ENOSPC` from the write that
  would exceed capacity.

Implement FUA as "release the buffer now, then complete", and stop rejecting it in
`decode_request`. Under a write-back buffer, FUA is not optional. It is the mechanism that lets
the buffer exist without breaking jbd2's ordering.

#### Buffer size and release shape

The buffer capacity is daemon configuration, allocated once at startup and never grown. Add
`--write-buffer-bytes`, default 4 MiB (1024 blocks), maximum 16 MiB (4096 blocks). Reject a value
that is not a multiple of `LOGICAL_BLOCK_BYTES` or that exceeds the maximum. The 16 MiB ceiling
exists because a release writes the whole buffer and then fsyncs it: a larger buffer turns one
FLUSH into an unacceptably long pause.

The default is 4 MiB rather than the maximum because buffer occupancy is set by how often ext4
and jbd2 send FLUSH, not by the buffer size. Under SQLite with WAL and `synchronous=FULL` the
flush interval is short, so the buffer is expected to release at tens of blocks, well below even
the 4 MiB default. That is still a 1.03x footer ratio, so the byte saving holds. This is a
prediction from the durability settings, not a measurement: the payloads-per-release histogram
decides the final default. Raise it only if the histogram shows releases hitting the cap.

A release is one or more records, not one record. Two limits apply:

- The footer holds at most 338 payloads, so a full 16 MiB buffer releases as 13 records.
- `writev` accepts at most `IOV_MAX` (1024) iovecs, so one 338-payload record with its footer
  fits in one call but a whole buffer does not.

So a release loops over records, one `writev` each, then issues exactly **one** `fsync` for the
whole release, then completes the pending request. The fsync count per FLUSH stays at one. That
is the number that matters.

Capacity note: reserving log blocks at insert time means a full 16 MiB buffer holds about 16.1 MiB
of reserved but unwritten log. Against the 128 MiB acceptance fixture that is an eighth of the
device, and against the 32-record volume used by ublk/fio scenario 7 the buffer is larger than the
whole device. Clamp the effective buffer to a fraction of device capacity when the volume opens,
and log the clamp. Do not take the flag literally on a small device.

### Change C: remove hot-path zeroing and copying

- Preallocate the record arena once per `Volume`, sized for the maximum record. Never
  zero-initialize it.
- `read_block` reads straight into its destination-backed aligned buffer instead of
  zero-initializing a 4 KiB stack block first.
- The checkpoint body uses a bounded, aligned 128 KiB buffer instead of the 4 KiB scratch block.
  Body submissions for this fixture fall from 96 to 3 with unchanged bytes and unchanged layout.

Steady-state target: no heap allocation on the read or write path, and constant resident memory
after startup. Verify with peak RSS across a long run, not by inspection.

### Cheap follow-on, only after A, B and C are measured

Raise the engine checkpoint interval from 64 MiB to 512 MiB of log bytes. Change B already cuts
log bytes roughly in half, so it also halves checkpoint frequency for the same workload. Measure
the interval change on its own, and record clean-restart and long-tail recovery time for the
512 MiB setting. Fewer pauses must not hide an unacceptable recovery cost.

## What this plan deliberately does not do

`UBLK_F_BATCH_IO`, `UBLK_U_IO_PREP_IO_CMDS`, `UBLK_U_IO_COMMIT_IO_CMDS` and
`UBLK_U_IO_FETCH_IO_CMDS` are out of scope. The reasoning, which is from the ABI rather than from
measurement:

The legacy transport already sends one `UBLK_U_IO_COMMIT_AND_FETCH_REQ` SQE per request, and with
queue depth greater than 1 those SQEs share a single `io_uring_enter`. Batch mode saves ring
bookkeeping per I/O, not syscalls. That cost is hundreds of nanoseconds against an `fsync` and a
128 KiB write. Batch mode also forbids `UBLK_U_IO_FETCH_REQ` and `UBLK_U_IO_COMMIT_AND_FETCH_REQ`
entirely, so adopting it rewrites the queue loop against `libublk`'s `UblkBatchQueue` and needs a
runtime feature probe plus a legacy fallback.

Reopen it only if, after A, B and C, a CPU profile shows queue transport above 5% of daemon CPU
time. Kernel support is present here (`/usr/include/linux/ublk_cmd.h`:356) and `libublk` 0.4.7
ships `io_batch.rs`, so the option stays available.

Also out of scope: multiple queues, zero-copy, `SQPOLL`, registered buffers, background
checkpointing, compaction, wraparound, and any format change.

CPU pinning is not in scope as code. `ublk_drv` runs request handling as `task_work` on the
daemon's own io_uring task, so there is no separate kernel thread to pin to. If migration looks
like a cost, test it with `taskset` on the daemon process and report the number. Do not pin the
database process to the same core as the daemon: that serializes two busy threads onto one CPU.

## Measurement

### Counters

Add opt-in counters and a bounded checkpoint event list. Not a tracing framework. Keep hot-path
counters in memory and emit summaries outside timed work.

| Measurement | Record |
|---|---|
| Bytes | Logical bytes received from ublk. Engine payload, footer, checkpoint body and descriptor bytes, each separately. Report `engine backing bytes / logical ublk write bytes` and `footer bytes / payload bytes`. |
| Batch occupancy | Payloads per released record, as a distribution. This is the primary evidence that Change B works. |
| Requests | Actual ublk request count and size distribution, and buffer releases by trigger: FLUSH, FUA, byte cap, payload cap. |
| Submissions | Backing operations and `submit_and_wait` calls by purpose. Daemon CPU time. |
| Checkpoints | Count, trigger, per-stage times, total pause and maximum pause. |
| Recovery and RAM | `open()` time split into map load, tail replay, stale-tail clear. Records and bytes replayed. Daemon peak RSS. |

SQLite, its WAL and ext4 journaling add layers above this. Do not call engine request bytes
physical-device write amplification.

### Workloads

Keep the pinned `speedtest1` and `kvtest` tools at 3.51.3 with the existing deterministic
adapter. Three cases:

- Mixed SQL: `speedtest1` main, with verification and individual subtest results.
- Bulk writes: seeded random 8 KiB BLOB updates in one transaction.
- Commit-heavy writes: the same dataset, seed and update count, one commit per operation.

Start with WAL/FULL, 4096-byte pages, the existing 1 MiB SQLite cache, mmap disabled, and a 4 MiB
dataset. Calibrate counts once for roughly 1 to 5 seconds of measured work inside the write
budget, then freeze them across every variant. Retest the final implementation with DELETE/EXTRA
and WAL/NORMAL before claiming the result holds across durability profiles.

go-ycsb is rejected for this spike. It measures SQLite through a Go `database/sql` driver and a
cgo boundary, its workloads are concurrency-throughput workloads while this device has one queue,
and the existing harness already owns the expensive parts: fixtures, write budgets, fsck gates
and evidence. Reconsider it for the ADR-06 RocksDB delivery, where one tool covering both engines
is worth the added layer. `workloada` (update-heavy) and `workloadc` (read-only) are the
candidates there.

### Baseline and control

Three paths on the same machine and the same backing medium:

| Path | Purpose |
|---|---|
| Native ext4 on a disposable loop image, no ublk, no engine | Control for the whole custom stack. |
| Ext4 over the unchanged engine, release build | Before measurement. |
| Ext4 over each candidate, same release build settings | After measurement. |

Check debug against release on one representative case first. A build-mode change is not a
batching improvement.

Both ext4 devices expose 128 MiB with 4 KiB logical sectors. Match mkfs features, journal
geometry and mount options. Record loop direct-I/O behavior and any unavoidable difference. The
installed helper permits only owned ublk fixtures, not loop devices: obtain explicit operator
approval for the native control and do not widen the helper. All database processes stay
unprivileged.

Run three fresh repetitions per case and path, alternating order. Report every sample plus median
and range: workload time, final drain, workload-plus-drain, throughput and bytes.

Write-budget note: the current 512 MiB cumulative ceiling cannot reach a 512 MiB checkpoint
interval. Give the follow-on interval experiment a 768 MiB ceiling on the 1 GiB backing, and
measure one interval crossing per fresh repetition.

## Correctness gates

Run after Change B and after any checkpoint change:

- The existing engine correctness and recovery tests.
- New focused tests: batch boundary at 1, 2, 337, 338 and 339 payloads; duplicate logical block
  address inside one buffer window; read-after-write with no intervening flush; `ENOSPC` returned
  by the write that exceeds capacity, not by the following flush; FLUSH completing only after the
  buffer is durable; partial and short backing writes.
- SQLite integrity check plus logical content equality across a clean daemon restart.
- `make test-ublk-fio`, unchanged. Scenario 5 (SIGKILL after a successful fio flush) and scenario
  7 (33-write exhaustion) are the two that a buffering bug breaks first.
- The full ext4/SQLite acceptance gate once, on the selected implementation, not per trial.

Data-loss warning: a write-back buffer means an unflushed write is lost if the daemon dies. That
is the volatile-cache contract and it is what a real disk cache does. It is not a licence to lose
a write that FLUSH already acknowledged. Any test that observes a flushed write disappearing is a
release blocker, not a tuning result.

## Deliverable

One results note: measured bottlenecks, per-change percentage deltas against the unchanged
release build, remaining distance to native ext4, accepted changes, rejected changes, and
remaining uncertainty. Keep or revert each change independently. Do not select on throughput
alone if pause, recovery, memory or correctness regress.

Keep one live workload at a time, 60-second command deadlines and a 600-second comparison-round
limit. Preserve raw commands, results and failed fixtures.
