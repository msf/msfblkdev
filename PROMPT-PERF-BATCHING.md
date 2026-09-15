# Agent prompt: request batching and write-back buffering

Paste the block below as the task for an implementing agent. It assumes the agent starts at the
repository root with a clean tree.

---

You are implementing a performance change in `/home/miguel/play/my-block-storage`, a Linux
log-structured block device with a ublk frontend. Rust is authoritative.

Read first, in this order, and do not start coding until you have:

1. `AGENTS.md` and `README.md`.
2. `PLAN-SQLITE-EXT4-PERFORMANCE.md`. This is your specification. Revision 2.
3. `rust/src/lib.rs` lines 112-275 (the `Volume` write, read and flush paths).
4. `rust/src/bin/block-storage-ublk.rs` lines 33-45, 292-330, 335-463, 545-620 (constants,
   target parameters, request decode and handling, queue loop).
5. `rust/src/checkpoint.rs` lines 17-167.

Load the `coding` and `rust-development` skills before writing any code.

## Scope

Three changes, in this order, each a separate commit with its own measurement:

- **A: wide request path.** Split `IO_BUFFER_BYTES` into `LOGICAL_BLOCK_BYTES` (4096) and
  `MAX_REQUEST_BYTES` (default 131072). Capacity arithmetic uses the logical block size.
  `max_sectors` is `MAX_REQUEST_BYTES >> 9` and the per-tag buffer is `MAX_REQUEST_BYTES`.
  Allocate one `IoBuf` per tag; the queue callback currently indexes `buffers[0]` and must index
  by tag. Split a multi-block request into logical blocks in the request handler.

  Two startup flags, one derived value:
  `--max-request-bytes` (default 131072; block multiple, page-aligned, at most 338 blocks so one
  request always fits in one log record) and `--inflight-memory-bytes` (default 4194304). Derive
  `depth = inflight_memory_bytes / max_request_bytes`, clamped to 1 through 4096, and log it with
  the total buffer memory. Do not expose depth as a flag.

  `QUEUE_COUNT` stays 1 and is not configurable. The engine has one serialized writer and one
  write-back buffer; a second queue adds a second thread contending for both. Do not derive queue
  count from the memory budget.

- **B: write-back buffer with FLUSH and FUA release.** A WRITE copies into a preallocated buffer
  indexed by logical block address and completes immediately. Repeated writes to one address
  overwrite in place. READ consults the buffer before the mapping table. FLUSH, a FUA write, a
  full buffer releases it. Buffer capacity is `--write-buffer-bytes`, default 4 MiB, maximum
  16 MiB, allocated once at startup and never grown, and clamped at volume open to a fraction of
  device capacity (the ublk/fio scenario 7 volume is 32 records, smaller than the default buffer). A release emits one record per 338 payloads
  (`writev` per record, bounded by the 338-payload footer limit and by `IOV_MAX` of 1024), then
  exactly one `fsync` for the whole release, then completes the pending request. The on-disk
  format does not change; the decoder already accepts up to 338 payloads under one footer. Stop
  rejecting FUA in `decode_request` and implement it as "release now, then complete".

- **C: hot-path allocation and zeroing.** Preallocate the record arena per `Volume` and never
  zero-initialize it. Remove the redundant payload copy and the zero-init in `read_block`. Give
  the checkpoint body a bounded aligned 128 KiB buffer in place of the 4 KiB scratch block,
  preserving byte order, checksums and layout.

## Two requirements that are easy to break

1. **`ENOSPC` must be reported by the write that exceeds capacity**, not by a later flush.
   Reserve log blocks when a payload enters the buffer. Log allocation is deterministic.
   `make test-ublk-fio` scenario 7 (33 writes into a 32-record volume) is the regression test.
2. **A write acknowledged by a completed FLUSH must survive a daemon kill.** Losing an
   unflushed buffered write is correct under the volatile write cache the device already
   advertises. Losing a flushed one is a release blocker. `make test-ublk-fio` scenario 5 is the
   regression test.

## Out of scope. Do not implement, do not propose mid-task.

`UBLK_F_BATCH_IO` and the `PREP`/`COMMIT`/`FETCH_IO_CMDS` transport. Multiple queues. Zero-copy.
`SQPOLL`. Registered buffers. CPU pinning as code. Background checkpointing. Compaction.
Wraparound. Any change to the on-disk record or checkpoint format. Raising the checkpoint
interval, which is a separate follow-on after A, B and C are measured.

## Tests you must add

Batch boundaries at 1, 2, 337, 338, 339 and 676 payloads, so a release crossing a record boundary
is covered. Configuration validation: a write-buffer size that is not a block multiple, one above
32 MiB, and a request size or depth whose product exceeds the memory budget. Duplicate logical block address inside one
buffer window. Read-after-write with no intervening flush. `ENOSPC` from the offending write.
FLUSH completing only after the buffer is durable. Partial and short backing writes.

## Gates

After each change:

```sh
cd rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cd ..
make lint test
```

After change B and after the checkpoint change, additionally:

```sh
make test-acceptance
make test-ublk-fio
```

`make test-ext4` and `make test-sqlite-ext4` need live mounts and operator approval. Ask before
running them; do not install, reinstall or widen the privileged helper.

## Measurement

Add opt-in counters, not a tracing framework. The primary evidence that change B works is the
distribution of payloads per released record, plus `footer bytes / payload bytes`. Also record
ublk request count and size distribution, buffer releases by trigger, `submit_and_wait` calls by
purpose, and daemon peak RSS.

Measure in release mode. Check debug against release on one case first, and never report a
build-mode difference as a batching result. Three fresh repetitions per case, alternating order,
reporting every sample with median and range.

## Working rules

- Do not mutate shared state. No pushes, no helper reinstalls, no changes outside the repository.
- If a change fights you, stop and report rather than adding complexity to force it.
- Report what you verified by running it, separately from what you inferred by reading code.
- Deliver one results note: per-change deltas against the unchanged release build, observed batch
  occupancy, bytes saved, correctness results, and anything you could not measure.
