# SQLite/ext4 performance spike

Status: proposed work-plan, 2026-09-13

Pause ADR-06 delivery work. Measure the cost of our block-device stack, try simple improvements, and compare SQLite before and after against ext4 without our device. SQLite is the primary workload. Finish with one bounded RocksDB cross-check, not a new database acceptance suite.

Checkpointing overhead is signifficant on larger values >=4GB, our checkpoitn interval of 64MiB is too short, it should be extended.
Keep filesystem and database durability settings unchanged. This plan includes changing **512 MiB engine checkpoint interval**. Include in the test workloads a form that allows observing the impact of our checkpointing overhead.

## What we already know

The accepted [SQLite run](evidence/sqlite-1789293925836503146/summary.json) used a debug Rust daemon, SQLite tools compiled with `-O2`, and tmpfs backing. These are starting observations, not the release-build or native-ext4 comparison we need.

WAL/FULL workload-plus-drain times, across three fresh repetitions:

| Existing case | Median | Range |
|---|---:|---:|
| `speedtest-main` | 49.38 ms | 48.83–57.04 ms |
| Random 8 KiB updates, 256 operations, one transaction | 16.76 ms | 15.11–23.28 ms |
| Same updates, one commit per operation | 207.15 ms | 183.46–215.32 ms |
| WAL stress, 4096 updates of two 8 KiB values | 1324.14 ms | 1312.93–1450.52 ms |

The correctness reference runs directly on tmpfs. It is **not a native-ext4 baseline**. The tiny datasets and warm caches also do not establish device-read performance.

Current design costs, derived from the source rather than measured as time:

| Component | Current behavior |
|---|---|
| Log record | One 4 KiB payload + one 4 KiB footer: **2× bytes**, before checkpoints or filesystem overhead. Already one `WRITEV`, not two submissions. |
| Engine checkpoint interval | 64 MiB of appended log bytes, including payloads and footers. This is separate from SQLite's 1000-frame automatic WAL checkpoint. |
| Checkpoint at 128 MiB logical capacity | 384 KiB mapping body + 4 KiB descriptor. The body uses **96 separate submit-and-wait operations**. Publication has three fsync stages. |
| Mapping arrays | 12 bytes per logical 4 KiB block: **384 KiB** for this device, excluding allocator, buffers and other daemon RAM. |
| Request handling | One ublk queue, depth one, maximum request 4 KiB. Each backing operation completes before the engine continues. |
| Existing record format | Recovery already accepts up to 338 payloads under one footer, with distinct logical block addresses. The live writer emits only one. |

Source anchors: [`checkpoint.rs`](rust/src/checkpoint.rs):17,50–167; [`lib.rs`](rust/src/lib.rs):112–127,196–275,308–463,633–737; [`block-storage-ublk.rs`](rust/src/bin/block-storage-ublk.rs):292–443.

## 1. Establish the costs and a small comparison

### Measure where time and bytes go

Add opt-in counters and a bounded checkpoint event list, not a tracing framework. Keep hot-path counters in memory and emit summaries outside timed work.

| Measurement | Record |
|---|---|
| Bytes written | Logical bytes received from ublk; engine payload, footer, checkpoint body and descriptor bytes separately; format and recovery-clear writes separately. Reconcile categories with the existing backing-write ledger. |
| Checkpoint pauses | Count and trigger reason (interval or close); log fsync, body encoding/checksum, body I/O, body fsync, descriptor I/O and descriptor fsync times; total pause and maximum pause. Record write latency around each checkpoint. |
| Submission overhead | Backing operations and submit/wait calls by purpose, actual request-size and batch-size distributions, flush count/time, daemon CPU time. Use one short CPU profile if attribution remains unclear. |
| Recovery and RAM | Engine `open()` time split into map loading, tail replay and stale-tail clearing; records/bytes replayed; map vector length/capacity bytes and daemon peak RSS. Keep this separate from filesystem check/remount time. |

Report both `engine backing bytes / logical ublk write bytes` and `footer bytes / payload bytes`. SQLite, its WAL and ext4 journaling add another layer: do not call engine request bytes physical-device write amplification.

Construct valid, flushed engine fixtures with empty, approximately 64 MiB and approximately 512 MiB log tails. Release them without the explicit checkpointing `Volume::close()`, then time reopening and verify contents. This measures replay without introducing a crash campaign. A normal SQLite clean restart alone hides this cost because daemon close checkpoints the map.

### Use three SQLite cases, not the 87-run matrix

Use pinned `speedtest1` and `kvtest` with the existing deterministic adapter:

- Mixed SQL: `speedtest1` main, with verification and individual subtest results.
- Bulk writes: seeded random 8 KiB BLOB updates in one transaction.
- Commit-heavy writes: the same dataset, seed and update count, with one commit per operation. Include enough work to observe repeated SQLite WAL checkpoints.

Start with WAL/FULL, 4096-byte pages, the existing 1 MiB SQLite cache and mmap disabled. For the paired update cases, start with a 4 MiB dataset so it exceeds SQLite's cache but fits comfortably on the fixture. Calibrate counts once, aiming for roughly 1–5 seconds of measured work within the write budget; then freeze them across every variant. If capacity prevents a longer case, report that limitation rather than running until full. Retest the winning implementation with DELETE/EXTRA and WAL/NORMAL before calling it useful across durability profiles.

Run three fresh repetitions per case and storage path, alternating their order. Report all samples, median/range, workload time, final drain, workload-plus-drain, throughput and bytes. Measure transaction tails only if a cheap bounded sampler supplies them; checkpoint pause maxima are useful immediately. Keep validation outside measured work, except the labeled upstream built-in verification.

### Establish a fair ext4 control

Compare these paths on the same machine and backing medium:

| Path | Purpose |
|---|---|
| Native ext4 on an owned disposable loop image, without ublk or our engine | Control for the whole custom-device stack. |
| Ext4 over the unchanged engine, built in release mode | Before measurement. |
| Ext4 over each candidate, with the same release build settings | After measurement. |

Start on tmpfs to match the current experiment and isolate software costs. Both ext4 devices expose 128 MiB with 4 KiB logical sectors. Match mkfs features, journal geometry and mount options; record loop direct-I/O/buffering behavior and any unavoidable path differences. This control measures the complete stack difference, not engine code alone. It does not predict NVMe performance. A later physical-storage comparison must put both paths on the same approved medium, not compare a RAM-backed engine with an SSD-backed control.

The installed helper permits only owned ublk fixtures, not loop devices. Obtain explicit operator setup approval for the native control; do not widen or reinstall that helper implicitly. All database processes remain unprivileged.

Reuse the existing tool commands, profiles, logical digests, deadlines and cleanup. Add only a small wrapper accepting the two owned workload directories and an explicit variant label. The current baseline comparator requires all 87 runs, so use a small subset report rather than expanding it into a benchmark framework. Native results have no engine ledger or mapping RAM: report those fields as not applicable.

**Deliverable:** one baseline table and one cost breakdown. First check debug versus release on a representative case; do not count a build-mode change as a batching improvement.

## 2. Make checkpointing cheaper, then use the 512 MiB interval

**Batch contiguous checkpoint body writes first.** Replace the 4 KiB scratch block with a bounded, aligned 128 KiB buffer. Preserve byte order, checksums and the on-disk layout. For this fixture, the expected body submission count falls from 96 to 3, with unchanged bytes. Start with one larger I/O at a time, not concurrent SQEs or background checkpointing. Compare at the existing 64 MiB interval so the effect remains visible.

Preserve publication order:

```text
complete prior log writes → fsync log
→ complete inactive checkpoint body → fsync body
→ complete descriptor → fsync descriptor → publish checkpoint cursor
```

**Raise the engine interval from 64 to 512 MiB of log bytes.** Record the setting explicitly. At today's one-payload records, that is 65,536 records / 256 MiB of payload between checkpoints. Close still checkpoints dirty mapping state. It is not a change to SQLite's automatic-checkpoint policy.

Measure interval-only, body-batching-only and the combination against the original. Use a small engine checkpoint/recovery diagnostic for attribution; run the SQLite shortlist on promising variants, not a large Cartesian product.

At this capacity, checkpoint body+descriptor bytes are about 0.592% of each 64 MiB log interval, versus 0.074% at 512 MiB. Footer bytes currently add 100% to payload bytes. These calculations suggest footer packing has more byte-saving potential; they do not establish which pause or CPU cost dominates.

The existing 512 MiB total-write ceiling cannot exercise the new interval. Give the separate spike a **768 MiB cumulative engine-write ceiling**, keeping the 1 GiB backing and reserving space for setup, recovery and cleanup. Use a calibrated fixed-count case that crosses one 512 MiB boundary, plus an engine-only diagnostic if SQLite cannot reach it safely. Two such intervals do not fit the current backing: measure one crossing per fresh repetition instead of quietly enlarging the fixture. Keep the accepted suite's limits unchanged.

**Deliverable:** SQLite impact, checkpoint count/pause distributions, and clean/long-tail recovery costs for the 512 MiB candidate. Fewer pauses must not conceal an unacceptable recovery trade-off.

## 3. Batch requests and payloads before adding concurrency

Implement and evaluate these changes separately, keeping one ordered engine writer:

| Experiment | Small first version | Expected benefit to verify |
|---|---|---|
| Larger ublk reads/writes | Raise maximum request to 128 KiB; retain 4 KiB logical blocks and depth one. Initially service the request through existing block operations. | Fewer kernel/userspace request round trips. No expected footer saving yet. |
| Multiple payloads under one footer | Pack the distinct blocks of one larger write request; start with at most 32 payloads. Reuse the existing format. | At 8 payloads, log bytes/payload bytes fall to 1.125×; at 32, to 1.03125×. Verify actual batch occupancy and bytes saved. |
| Bounded request and backing-I/O batching | Once the engine accepts batches, try queue depth 8 and drain already-ready work up to a byte/count cap. Pack compatible writes; combine physically contiguous reads, otherwise use bounded separate read submissions. | Fewer submit/wait cycles and useful queued work, without a thread pool or artificial batch-fill delay. |
| Native ublk batch transport | If dispatch remains material, probe kernel support for `UBLK_F_BATCH_IO` and evaluate the pinned libublk `UblkBatchQueue` / `UblkBatchConfig` APIs. | Amortized fetch/completion overhead. These APIs do not batch backend I/O automatically. |

Two implementation traps need explicit tests. `IO_BUFFER_BYTES` currently also participates in reported device capacity: separate request capacity from logical block size before raising it. The lab's sector-count-divided-by-eight metric becomes a count of 4 KiB units, **not requests**, once requests grow; collect actual request counts and sizes instead.

Preserve flush ordering and read-after-write behavior. Finish earlier writes before acknowledging a flush; do not move later writes across it. Publish mappings and complete requests only after the required backing completion. Split batches at overlapping logical addresses, since the decoder rejects duplicate addresses within one record. Keep FUA unsupported unless its semantics are deliberately implemented; it is not a performance switch.

The accounting writer currently permits one in-flight operation. Start with a single bounded combined write where possible. Before multiple SQEs, reserve the whole batch and handle every completion, short I/O and uncertain failure without bypassing the write budget. Own buffers until completion and bound queue memory. Logical adjacency does not imply physical adjacency after overwrites.

If SQLite reads stay in caches, use one bounded direct-read diagnostic on the owned device to evaluate read batching. Do not turn zero device reads into a read-throughput claim or change global caches to manufacture traffic.

**Deliverable:** per-change throughput, bytes, CPU, observed request/batch sizes and latency impact. A deeper queue is useful only if work actually accumulates and the engine can consume it safely.

## 4. Keep only improvements supported by the numbers

- Run the existing engine correctness/recovery tests after changing checkpoint or record publication, plus focused batch-size, boundary, duplicate-address, flush-order and partial-I/O tests. Require SQLite integrity and logical-content equality before and after clean restart. Run the full ext4/SQLite acceptance gate once on the selected implementation, not on every trial.
- Report baseline-to-candidate percentage changes and remaining distance to native ext4. Interleave additional runs when differences overlap the observed variation. Retain or revert each experiment separately; do not select on throughput alone if pauses, recovery, memory or correctness regress.
- If profiling justifies them, consider symmetric batching of checkpoint reads/stale-tail clearing, reusable buffers, or eliminating provably redundant fsyncs. A log LSN alone cannot prove a flush redundant: checkpoint body and descriptor writes also need synchronization. Defer zero-copy, polling modes, multiple queues, background checkpoints, compaction and format redesign.
- Cross-check the winner with one small, pinned RocksDB `db_bench` overwrite/read-back case on both paths: fixed seed/count, one thread, WAL enabled and synchronous writes. Time initialization and final flush/close separately and verify contents. Report it independently from SQLite; do not build the ADR-06 RocksDB matrix during this spike.

Keep one live workload at a time, 60-second command deadlines and a 600-second comparison-round limit. Preserve raw commands/results and failed fixtures. Deliver one concise results note identifying measured bottlenecks, accepted changes, rejected ideas and remaining uncertainty. The first implementation task is measurement and the native-ext4 control, not a wholesale request-path rewrite.
