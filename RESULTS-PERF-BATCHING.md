# Results: request batching and write-back buffering

Status: implemented and gated, 2026. Covers changes A, B and C from
`PLAN-SQLITE-EXT4-PERFORMANCE.md` (revision 2). Live ext4/SQLite throughput
comparison is not included; it needs operator approval for mounts (see
"Not measured").

## Outcome

All three changes are implemented, each in its own commit, and pass every gate,
including the live-mount gates:

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
- `make lint test`
- `make test-acceptance` (exhaustive engine crash matrix, 20 repetitions per
  boundary, including every checkpoint-body block boundary)
- `make test-ublk-fio` (all seven live scenarios, three repetitions each)
- `make test-ext4`, `make test-xfs` (live filesystem lifecycles)
- `make test-sqlite`, `make test-sqlite-ext4` (87 of 87 matrix runs, 0 failures)
- `make lint-zig`, `make test-zig` (historical Zig mirror)

The first `make test-sqlite-ext4` run after change A failed, and the cause was a
stale lab invariant rather than the engine. The lab derived a request count from
the kernel's data sectors by dividing by eight, which assumed one request per
4 KiB block. That identity held only while `max_request_bytes` was 4096. The
invariant now compares sectors and flushes, which a wide request path does not
distort, and which check the transferred quantity rather than a count.

The two hard requirements hold, verified by running them:

- `ENOSPC` is returned by the write that exceeds capacity, not a later flush.
  Log blocks are reserved when a payload first enters the buffer and are never
  reclaimed by batching, so a 32-record volume still fails the 33rd write.
  Regression: `make test-ublk-fio` scenario 7 and the engine test
  `enospc_is_returned_by_the_offending_write_not_a_later_flush`.
- A write acknowledged by a completed FLUSH survives a daemon kill. Regression:
  `make test-ublk-fio` scenario 5 and the engine test
  `buffered_write_is_volatile_until_flush_across_sigkill`, which also confirms
  an unflushed buffered write is correctly lost.

## What changed

- **A — wide request path.** `IO_BUFFER_BYTES` split into `LOGICAL_BLOCK_BYTES`
  (4096) and `MAX_REQUEST_BYTES` (default 131072, at most 338 blocks so one
  request always fits one log record). `--max-request-bytes` and
  `--inflight-memory-bytes` serve flags; depth is derived
  (`inflight / max_request`, clamped 1..=4096) and logged, never a flag.
  `QUEUE_COUNT` stays 1. One `IoBuf` per tag; the queue callback indexes by tag;
  multi-block requests split into logical blocks in the handler.
- **B — write-back buffer.** A WRITE copies into a preallocated, 4 KiB-aligned
  buffer indexed by logical block address and completes immediately; repeats to
  one address coalesce in place. READ consults the buffer before the mapping
  table. FLUSH, FUA, or a full buffer releases the buffer as one record per 338
  payloads (one `writev` each) then exactly one `fsync`. Auto-releases from a
  full buffer do not `fsync`; durability comes from flush, FUA or a checkpoint,
  so the fsync count per FLUSH stays one. The on-disk format is unchanged.
  The library default is a one-block write-through window; the daemon opts into
  4 MiB via `--write-buffer-bytes`, clamped at open to a quarter of the device
  log (a small device collapses toward write-through).
- **C — hot-path allocation and zeroing.** Preallocated aligned read scratch
  and a preallocated `writev` iovec scratch per volume: no per-operation heap
  allocation and no per-block zeroing on the read or release path. The
  checkpoint body streams through a 128 KiB aligned buffer, collapsing per-block
  submissions into 32-block reads and writes (96 to 3 on the ext4 fixture) with
  unchanged byte order, checksums and layout.

## Batch occupancy (measured, live ublk/fio)

Opt-in counters emitted by the daemon at shutdown. `backing_over_payload` is
`(footer_bytes + payload_bytes) / payload_bytes`; write-through is 2.000.

| Scenario | payloads/record | backing/payload |
|---|---|---|
| Sequential 16-block writes (one FUA/flush window) | 16.000 | 1.062 |
| Engine test, 340 payloads, one release | 338 then 2 | ~1.006 |
| Overwrite, `fsync` after every write | 1.000 | 2.000 |

The overwrite case shows the expected floor: when the filesystem flushes after
every write there is nothing to batch, so the footer ratio stays at 2.000. The
sequential case shows the win: 16 logical writes become one record.

Byte arithmetic (calculated, not a device measurement): footer overhead falls
from 2.000x of payload bytes to 1.125x at 8 payloads per record, 1.031x at 32,
and 1.003x at 338. Syscalls per release fall from two per write to two per
release (one `writev` per 338-payload record plus one shared `fsync`).

## Tests added

- Engine (`rust/src/tests/write_buffer.rs`): batch boundaries at 1, 2, 337, 338,
  339 and 676 payloads (crossing a record boundary); duplicate address inside
  one window coalesces; read-after-write with no flush; `ENOSPC` from the
  offending write; short backing write fails the release without publishing;
  checkpoint body round-trips across the 128 KiB chunk boundary; release-stats
  occupancy and footer ratio.
- Crash (`rust/src/tests/process_crash.rs`): flushed write survives SIGKILL and
  unflushed buffered write is lost.
- Daemon (`rust/src/bin/block-storage-ublk.rs`): serve-config validation and
  derived depth; multi-block request splitting; FUA accepted on writes and
  rejected on reads/flushes.

## Not measured

The full throughput comparison in the plan — native ext4 on a loop image, ext4
over the unchanged release build, and ext4 over the batched build, three fresh
repetitions each in release mode — is not run here. The gates above prove
correctness under the batched path, not a speedup against native ext4. All
numbers in this note come from debug builds, so they are not throughput claims.
Peak-RSS-over-a-long-run confirmation of constant resident memory also needs a
live workload.

Debug-versus-release control and the checkpoint-interval follow-on (64 MiB to
512 MiB) are deliberately deferred: the follow-on is a separate experiment after
A, B and C are measured against native ext4.
