# ADR-01 Log-structured block device

Date: 2026-08-29
Author: Miguel Filipe
Status: accepted
On-disk format: 1

## Context

This project builds a Linux userspace block device backed by a local log-structured storage engine. We want correctness evidence before concurrency, performance work or distribution.

Linux exposes the device through ublk. A later goal mounts ext4 or XFS on that device so an existing application can use it unchanged.

```text
Application
  ↓
Filesystem
  ↓
/dev/ublkbN
  ↓
Linux ublk driver                 kernel
════════════════════════════════════════
ublk userspace daemon             userspace
  ├─ block request handling
  ├─ log-structured storage engine
  └─ backing file or logical volume
```

The first implementation experiment used Zig. It proved the low-level design through V0.4. The Rust implementation then proved the same behavior with simpler ownership of aligned buffers and `io_uring` lifetimes. The two persistent encodings have already diverged.

## Decision

Rust is the only implementation we will evolve. Its persistent encoding is authoritative from this ADR onward. Zig remains a historical experiment and test reference. We do not promise Zig and Rust image compatibility, and future Rust work does not update the Zig implementation.

We will:

1. Prioritize crash recovery and explicit durability semantics.
2. Use deterministic tests and machine-verifiable delivery gates.
3. Keep one serialized writer until correctness is proven through the ublk stack.
4. Add concurrency and performance work only after filesystem workloads are reliable.

The storage engine uses Linux `io_uring`, direct I/O and explicit aligned buffers. It does not use an async runtime. The ublk frontend can use the maintained Rust ublk support required to implement the kernel protocol.

The project remains local and single-node. Remote transport, replication, multiple backing stores, live migration, compaction and Byzantine fault tolerance are not current commitments.

A local file or dedicated logical volume is sufficient for the backing store. The important artifact is the engine and its correctness evidence, not an exotic backend.

## Local storage-engine design

This section specifies on-disk format version 1 and its recovery invariants. `V0` names the implementation milestone family, not the on-disk version. The goal ADRs define when each behavior becomes required.

The engine uses a pre-sized file or raw block device through `io_uring`. It supports one serialized writer and has no compaction or log wraparound. The caller creates and pre-sizes the backing store. The formatter formats it in place.

### Bounds and layout

- Logical block size: 4 KiB.
- Maximum virtual volume: 8 TiB = `2^31` logical blocks. Logical block address (LBA) IDs and the volume block count use `u32`. An LBA ID must have its high bit zero. The volume block count can equal `2^31`. Validate a logical range with `start <= volume_blocks` and `count <= volume_blocks - start`. Byte sizes, byte offsets, ublk's 512-byte sector addresses and local sequence numbers (LSNs) use `u64`.
- Physical addresses are 4 KiB block indexes encoded as `u32`; physical block zero never stores payload and is the unmapped sentinel in the LBA map.
- All persistent integers are little-endian. Persistent structures are encoded explicitly rather than written from compiler-layout structs.
- Each checkpoint descriptor starts with the `VBLC` magic bytes. Each write footer starts with the `VBLF` magic bytes. Both structures store on-disk format version `1` in the following 8-bit field. Readers reject unsupported versions before interpreting the remaining fields.

```text
4 KiB physical block 0          green checkpoint descriptor
4 KiB physical block 1          blue checkpoint descriptor
green checkpoint body           physical-block map, then checksum map
blue checkpoint body            physical-block map, then checksum map
log                              payload blocks followed by one footer per record
```

For `N = volume_bytes / 4096`:

```text
RAM mapping                  N × 12 bytes
physical-map bytes           ceil(N × 4 / 4096) × 4096
checksum-map bytes           ceil(N × 8 / 4096) × 4096
one checkpoint body          physical-map bytes + checksum-map bytes
both checkpoint bodies       2 × one checkpoint body
```

The allocation follows the configured volume size, not the 8 TiB format maximum. At 4 TiB the mapping uses 12 GiB of RAM; at 8 TiB it uses 24 GiB. The backing store must also have room for both checkpoint bodies and the finite V0 log.

### Checksums

On-disk format version 1 uses XXH3-64 checksums. The format version defines the checksum algorithm and its inputs. These checksums detect accidental corruption. They do not authenticate data from a malicious block client.

A payload checksum binds the volume, logical and physical addresses to the bytes:

```text
XXH3-64(seed=volume_id, little_endian(lba, physical_block) || payload[4096])
```

Metadata checksums use unseeded XXH3-64 over the complete 4 KiB structure with its checksum field set to zero. Unused array entries and alignment padding must be zero.

A checkpoint-body checksum uses unseeded XXH3-64 over every padded physical-map block followed by every padded checksum-map block. An initial empty checkpoint stores a zero body checksum and has no body to read.

### Write-record footer

Each record is contiguous payload followed by exactly one 4 KiB footer:

```text
[payload block 0] ... [payload block N-1] [footer]
```

The footer has fixed-position, zero-padded arrays. Entry `i` describes payload block `i`.

| offset | size | field |
|---:|---:|---|
| 0 | 4 | magic (`VBLF`) |
| 4 | 1 | format version (`1`) |
| 5 | 1 | record kind (`write = 1`) |
| 6 | 2 | zero padding |
| 8 | 8 | volume ID |
| 16 | 8 | local sequence number |
| 24 | 4 | previous footer physical block |
| 28 | 4 | this footer's expected physical block |
| 32 | 1352 | `lba_ids[338]`, each `u32` |
| 1384 | 2704 | `checksums[338]`, each `u64` |
| 4088 | 8 | footer checksum |

The footer bounds a record to 338 payload blocks. This is 1.3203125 MiB of payload and 339 physical blocks including the footer. In this ADR, `maximum_record_blocks` is therefore 339. The payload count is `footer_block - previous_footer_block - 1`. The initial checkpoint uses `log_start_block - 1` as the previous-footer boundary sentinel. LBA IDs must be unique within a record.

V0 writes one payload block per record. V0.6 recovery must nevertheless accept every valid format-version-1 record containing 1 to 338 payload blocks. Later batching can then use the existing encoding without changing recovery.

There are no speculative reserved fields. A future footer version can add replication terms. Separate record kinds can represent membership changes. Checkpoint state can contain durable, committed and applied watermarks if replication requires them. Async writes require runtime state, not new footer fields. Compaction, compression or a smaller footer requires a new format version.

V0 precomputes the footer. It submits the payload and footer with one `io_uring` `WRITEV` operation. The final iovec contains the footer. V0 publishes the mapping only after an exact-length completion. The footer contains framing and checksum metadata; it is not a commit marker. A torn record can contain a valid footer and a corrupted payload. `read_block` detects that corruption later.

### In-memory state

`Volume` owns the backing I/O state, the fixed-size structure of arrays (SoA), and the mutable recovery and checkpoint cursors:

```text
Volume
  backing_fd
  io_uring
  volume_id                 u64
  volume_blocks             u32
  log_start_block           u32
  physical_blocks[N]        u32    # zero means unwritten/discarded
  checksums[N]              u64    # zero when physical_blocks[i] is zero
  last_lsn                  u64
  durable_lsn               u64
  checkpoint_lsn            u64
  last_footer_block         u32
  next_checkpoint_slot      green | blue
  log_bytes_since_checkpoint u64    # added with bounded recovery in V0.6
```

No LBA is stored in the mapping because its array index is the LBA. With one serialized writer, the next append block is always `last_footer_block + 1` and is not stored separately.

### Checkpoints

Green and blue are fixed checkpoint slots used alternately. Their 4 KiB descriptors live at physical blocks zero and one; their body regions are reserved at format time.

A descriptor has this exact layout:

| offset | size | field |
|---:|---:|---|
| 0 | 4 | magic (`VBLC`) |
| 4 | 1 | format version (`1`) |
| 5 | 1 | checkpoint slot (`green = 0`, `blue = 1`) |
| 6 | 2 | zero padding |
| 8 | 8 | volume ID |
| 16 | 4 | logical block size (`4096`) |
| 20 | 4 | volume block count |
| 24 | 8 | backing block count |
| 32 | 8 | checkpoint LSN |
| 40 | 4 | last footer physical block |
| 44 | 8 | checkpoint-body checksum |
| 52 | 4036 | zero padding |
| 4088 | 8 | descriptor checksum |

The slot and volume geometry determine the checkpoint-body location and length. The body is an exact snapshot of `physical_blocks` followed by `checksums`. Each array is rounded independently to 4 KiB.

An initial descriptor has checkpoint LSN zero, `last_footer_block = log_start_block - 1`, and body checksum zero. This state implies an empty mapping, so format does not write an all-zero checkpoint body. Startup does not read the body for this initial state.

V0 creates a checkpoint synchronously:

1. Stop new writes.
2. Drain the current record.
3. Set `S` to the current LSN.
4. Flush the log through `S`.
5. Write the inactive slot's mapping body.
6. Flush the mapping body.
7. Write the inactive slot's descriptor with `S`, the footer position and the body checksum.
8. Flush the descriptor.
9. Select the slot as active in memory.
10. Set `checkpoint_lsn` to `S`.
11. Reset `log_bytes_since_checkpoint`.
12. Resume writes.

Checkpoint LSN orders the two slots. A higher LSN contains newer logical state. Equal LSNs must describe the same mapping because V0 has no compaction. Startup can select either equivalent slot. It refuses to open if two valid equal-LSN slots have different body checksums or immutable geometry.

Each valid descriptor is a checkpoint candidate. Startup validates a candidate's complete body before selecting it. If the newer candidate is invalid, recovery tries the older candidate. One valid candidate is sufficient. Startup refuses to open if no candidate is valid or if valid candidates have conflicting volume identity or geometry.

Through V0.5, `close` is the only required checkpoint trigger. If `last_lsn == checkpoint_lsn`, the mapping has not changed and `close` does not write another checkpoint.

V0.6 adds `checkpoint_after_bytes`, measured as physical log bytes after the selected checkpoint. `open` uses a 64 MiB default. An options-based open call lets tests and the daemon select another value. The value must be 4 KiB aligned and at least one maximum-sized record (339 blocks).

Before accepting a record that would make `log_bytes_since_checkpoint > checkpoint_after_bytes`, the engine creates a checkpoint. A record that reaches the bound exactly is accepted. This keeps the replay tail at or below the configured bound.

### Operations

`format(backing, volume_size)`:

1. Validate 4 KiB alignment, the 8 TiB limit and backing capacity.
2. Compute both checkpoint body regions and the log start.
3. Write two valid empty checkpoint descriptors with LSN zero and `last_footer_block = log_start_block - 1`.
4. Flush the descriptors.

There is no zero-payload format record. The two checkpoint descriptors are the format roots, and the log starts empty. On first open, the implementation selects either equivalent initial slot deterministically.

`open(backing)`:

1. Open with direct I/O and initialize `io_uring` and aligned buffers.
2. Recover a checkpoint and its log tail as described below.
3. Expose the volume only after recovery completes.

`write_block(lba, data[4096])`:

1. Validate the LBA.
2. Reserve the next payload block, footer block and LSN.
3. Compute the payload checksum.
4. Encode the complete footer.
5. Submit the payload and footer as one `io_uring` `WRITEV` operation.
6. Require an exact-length completion.
7. Update both mapping arrays and the log cursors.

A failed or short backing I/O operation puts the volume in a failed state without publishing the new mapping. No later read, write or flush operation is valid in this state. The caller can only release the volume. Validation and finite-log capacity errors happen before I/O and do not poison the volume.

`read_block(lba, data[4096])`:

1. Validate the LBA.
2. Read its physical block and checksum from the arrays.
3. Return zeros if the physical block is zero.
4. Otherwise submit the 4 KiB read.
5. Recompute and compare the checksum.
6. Return the data or a checksum-mismatch error.

V0 detects payload corruption but cannot repair it.

`flush()`:

1. Drain all submitted payload and footer writes.
2. Submit an `io_uring` fsync against the backing file descriptor.
3. After successful completion, advance `durable_lsn` and complete the flush.

If `last_lsn > checkpoint_lsn`, `close()` creates a checkpoint, which includes the log flush. Otherwise, `close()` only flushes the log. It then closes the ring and backing file descriptor. A flush or checkpoint failure makes `close` fail.

### Crash recovery

Startup reads both checkpoint descriptors. It considers valid candidates in descending checkpoint-LSN order. For each candidate, startup reads the body into the mapping arrays and verifies the complete body checksum. If a body is invalid, startup tries the next candidate. If no candidate is valid, V0 refuses to open. Full-log salvage is a separate future tool.

The following tail-recovery algorithm is the V0.6 target. Earlier milestones reject a valid unreplayed tail.

Tail recovery starts at `last_footer_block + 1`. The selected checkpoint supplies the expected LSN and footer position:

1. Probe each candidate footer from `previous_footer_block + 2` through `previous_footer_block + 339`, bounded by the backing store. These positions represent 1 to 338 payload blocks.
2. Derive the payload count from the candidate and previous footer positions. Accept only a footer whose metadata checksum, volume, LSN, self-position, previous-footer link and LBA bounds all match expectations.
3. Apply each LBA's derived payload position and checksum to the arrays, then continue after that footer.
4. If no valid footer exists within one maximum record, stop. Ignore records after this gap.
5. Zero and flush `min(339, backing_blocks - append_block)` blocks from the recovered append position so stale records cannot reappear.
6. Set `last_lsn` and `last_footer_block` to the final replayed record. Set `durable_lsn = last_lsn` after the stale-tail flush. Keep `checkpoint_lsn` from the selected checkpoint and set `log_bytes_since_checkpoint` to the replayed physical bytes.
7. Serve requests only after these steps complete.

Recovery does not read or verify payload data. A valid footer with corrupted payload is accepted into the mapping; corruption is detected if that LBA is later read.

### V0 fault model

V0 assumes one writer and a backing store where a successful fsync makes all prior writes durable. It covers:

- incomplete or torn records: an invalid footer truncates the tail; a valid footer with damaged payload is accepted and detected lazily when that LBA is read;
- corrupted footer/checkpoint metadata: checksum failure, tail truncation or fallback to the other checkpoint;
- corrupted payload writes or later bit flips: detected lazily by `read_block` and returned as an error;
- reported backing I/O errors: propagated without publishing the affected mapping.

V0 does not repair corruption, protect against maliciously forged footer data, compact or wrap the log, retry failed writes, support discard or write-zeroes, or provide concurrent writes. ADR-03 tests process crashes. Later goals add deterministic medium-fault injection.

## Delivery roadmap

Every implementation delivery extends this Rust gate:

```sh
cd rust
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

A delivery is complete only when its gate and named acceptance tests pass without weakening earlier tests.

### Goal 1: basic read and write

[ADR-02](ADR-02-GOAL-1-BASIC-READ-WRITE.md) records V0.0 through V0.4:

- aligned direct I/O and exact completion handling;
- format and open from two checkpoint roots;
- append, flush and clean checkpoint publication;
- zero reads and checksummed reads before and after reopen.

Goal 1 is complete. V0.4 behavior and its Rust closure gate are green.

### Goal 2: minimum credible device

[ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) is the active goal. It delivers:

1. V0.5 multiple-write and overwrite semantics.
2. V0.6 bounded crash-tail recovery, including interrupted checkpoint publication.
3. V0.7 serialized 4 KiB ublk frontend supporting READ, WRITE and FLUSH.
4. V0.8 vertical `fio` validation through graceful restarts and deterministic hard process termination.

Automated tests use `SIGKILL` at coordinated points to model process-level fail-stop without cleanup. This covers user `kill -9` and OOM termination from the process's perspective. It does not model loss of the kernel, controller cache or power while an I/O is in flight. The V0.6 durability contract therefore depends on the backing device honoring successful fsync.

### Goal 3: backing-medium fault resilience

Keep ublk and `fio` as the vertical workload while injecting deterministic, non-adversarial backing faults:

- `EIO`, short I/O, timeouts, `ENOSPC` and device disappearance;
- torn writes, bit flips, stale reads, misdirected I/O and reordering;
- errors during normal I/O, checkpoint publication and recovery.

The daemon must remain alive and diagnosable. The affected volume fails closed or transitions offline, and ublk completes requests with an error instead of hanging. We will evaluate `dm-flakey`, `dm-log-writes`, `dm-error` and existing `fio` facilities before writing a custom faulting backend.

Deterministic describes the test mechanism. These faults need not be deterministic on real hardware.

### Goal 4: filesystem workloads

Once medium faults have deterministic behavior:

1. Create filesystems with `mkfs.ext4` and `mkfs.xfs`.
2. Mount each filesystem, create and update files, call fsync, and verify hashes.
3. Unmount, restart the daemon, remount and verify the same data.
4. Repeat the applicable process-crash and medium-fault cases through the mounted stack.

Database workloads remain optional until filesystem semantics are reliable.

### Goal 5: measured performance work

Only after the functional base is robust:

- establish `fio` latency, throughput and CPU baselines;
- support multi-block requests and batching;
- increase queue depth and add controlled concurrency;
- profile before changing the format or adding caching;
- add compaction or log wraparound when finite-log exhaustion blocks longer workloads.

### Deferred fault model

Byzantine storage is not nondeterministic storage. A Byzantine device can forge self-consistent blocks, checksums or old valid state. XXH3 detects accidental corruption but cannot authenticate data or prevent replay. Byzantine tolerance requires a separate design with keyed authentication, trusted monotonic state or replication, and is not on the current roadmap.

## Consequences

The first ublk device will be intentionally slow and finite. One queue, one outstanding request and one-block records make behavior easier to prove. We accept this limit until the mounted-stack tests are reliable.

Zig images may become unreadable by Rust and vice versa. This is acceptable while no persistent compatibility contract or user data exists.

## Why this project

- Directly addresses Railway's published exercise: **“Design a Storage Engine to power something like Railway's Volumes.”**
- Aligns with TigerBeetle's Zig, TigerStyle, deterministic simulation and storage-correctness focus.
- Produces current, public evidence of low-level systems work.
- Gives a compelling demonstration: run a real filesystem and database on the device, then validate recovery under injected faults.

## References

- Railway storage role and exercise: https://railway.com/careers/platform-engineer-storage
- Railway Volumes semantics: https://docs.railway.com/volumes/reference
- TigerBeetle: https://tigerbeetle.com/
- TigerStyle: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md
- Tigerlings: https://github.com/tigerbeetle/tigerlings
- Zig 0.16.0 release notes: https://ziglang.org/download/0.16.0/release-notes.html
