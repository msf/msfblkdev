# Coding project for TigerBeetle and Railway

## Project thesis

A userspace, log-structured virtual block device designed for deterministic fault testing and later quorum replication, initially backed by one local device.

Linux exposes the device locally (for example, `/dev/my-vol`) through ublk. A local filesystem such as ext4 or XFS can mount it, allowing an existing database such as PostgreSQL or a KV store to run unchanged on top.

```text
Database / KV store
  ↓
Filesystem
  ↓
/dev/my-vol
  ↓
Linux ublk driver                 kernel
════════════════════════════════════════
ublk userspace daemon             userspace
  ├─ block protocol handling
  ├─ log-structured storage engine
  └─ backing store (initially a file or raw device)
```

## Emphasis

1. Correctness under concurrency and failures, including log recovery.
2. An explicit fault model, deterministic simulation and fault injection.
3. A log structure that makes later replication and HA easier to reason about.
4. Sequential physical writes and random reads, appropriate for NVMe-backed storage.
5. A future distributed form as a replicated state machine/log, including quorum commit and fencing.

## Language and engineering style

- **Zig 0.16.0 is pinned.** The project-local compiler is downloaded from ziglang.org and excluded from Git.
- Zig 0.16's high-level `std.Io.Uring` backend is explicitly unfinished. Before committing to Zig beyond v0.0, implement a bounded spike using the low-level `std.os.linux.IoUring`: open a direct-I/O backing device, perform aligned read/write/fsync operations, validate CQE errors and short I/O, then reopen and verify the data.
- If that direct `io_uring` path is not small and trustworthy, use Rust with the low-level `io-uring` crate. I/O ergonomics are a valid language-selection criterion; do not build an async runtime as part of this project.
- Do not use Go: we want tight control of memory and using a *real* systems language. (muahahaha!)
- Follow TigerStyle where applicable: simple control flow, bounded resources, assertions, checksums and an explicit fault model. Apply the principles rather than copying constraints without context.

## Scope boundary

The initial project is local and single-node. Remote transport, replication, multiple backing devices, live migration, compaction strategy and performance optimisation are potential extensions—not current commitments.

The backing store can theoretically be anything that satisfies the block contract, but a local file or raw block device keeps the project focused. The important artifact is the storage engine and its correctness evidence, not an exotic backend.

## V0 plan

V0 is the standalone local storage engine, before ublk integration. It uses a pre-sized file or raw block device through `io_uring`, supports one serialized writer, and has no compaction or log wraparound. A raw block device is formatted rather than created.

### Bounds and layout

- Logical block size: 4 KiB.
- Maximum virtual volume: 8 TiB = `2^31` logical blocks. LBA IDs and the volume block count use `u32`; their high bit must be zero. Logical ranges are checked with `count <= volume_blocks - start`. Byte sizes, byte offsets, ublk's 512-byte sector addresses and LSNs use `u64`.
- Physical addresses are 4 KiB block indexes encoded as `u32`; physical block zero never stores payload and is the unmapped sentinel in the LBA map.
- All persistent integers are little-endian. Persistent structures are encoded explicitly rather than written from compiler-layout structs.

```text
4 KiB physical block 0          green checkpoint descriptor
4 KiB physical block 1          blue checkpoint descriptor
green checkpoint body           physical-block map, then checksum map
blue checkpoint body            physical-block map, then checksum map
log                              payload blocks followed by one footer per record
```

For `N = volume_bytes / 4096`:

```text
RAM mapping                     N × (4 + 8) bytes
one checkpoint body             N × (4 + 8) bytes, rounded to 4 KiB
both checkpoint bodies          N × 24 bytes, rounded to 4 KiB
```

The allocation follows the configured volume size, not the 8 TiB format maximum. At 4 TiB the mapping uses 12 GiB of RAM; at 8 TiB it uses 24 GiB. The backing device must also have room for both checkpoint bodies and the finite V0 log.

### Checksums

V0 uses versioned XXH3-64 checksums. Zig 0.16 provides `std.hash.XxHash3`; a Rust implementation must produce the same pinned test vectors. These checksums detect accidental corruption but are not authentication against a malicious block client.

A payload checksum binds the volume, logical and physical addresses to the bytes:

```text
XXH3-64(seed=volume_id, little_endian(lba, physical_block) || payload[4096])
```

Metadata checksums cover the complete 4 KiB structure with its checksum field set to zero. Unused array entries and alignment padding must be zero.

### Write-record footer

Each record is contiguous payload followed by exactly one 4 KiB footer:

```text
[payload block 0] ... [payload block N-1] [footer]
```

The footer has fixed-position, zero-padded arrays. Entry `i` describes payload block `i`.

| offset | size | field |
|---:|---:|---|
| 0 | 4 | magic (`VBLF`) |
| 4 | 1 | format version |
| 5 | 1 | record kind (`write` in V0) |
| 6 | 2 | flags, zero in V0 |
| 8 | 8 | volume ID |
| 16 | 8 | local sequence number |
| 24 | 4 | previous footer physical block |
| 28 | 4 | this footer's expected physical block |
| 32 | 1352 | `lba_ids[338]`, each `u32` |
| 1384 | 2704 | `checksums[338]`, each `u64` |
| 4088 | 8 | footer checksum |

The footer therefore bounds a record to 338 payload blocks: 1.3203125 MiB of payload and 339 physical blocks including the footer. Its payload count is derived as `footer_block - previous_footer_block - 1`. The initial checkpoint uses `log_start_block - 1` as the previous-footer boundary sentinel. LBA IDs must be unique within a record. V0 writes one block per record; the format leaves batching available without changing recovery.

There are no speculative reserved fields. Replication terms would belong to a future footer version, membership changes to separate record kinds, and durable/committed/applied watermarks to checkpoint state. Async writes require runtime state rather than new footer fields; compaction, compression or a smaller-footer variant would define a new versioned layout.

V0 precomputes the footer and submits one `io_uring` `WRITEV` operation whose final iovec is the footer. It publishes the mapping only after one exact-length completion. The footer is framing and checksum metadata, not a commit marker: a power-torn record may leave a valid footer with corrupted payload, which is detected lazily on read.

### In-memory state

`Volume` owns the backing I/O state, the fixed-size Struct of Arrays, and the mutable recovery/checkpoint cursors:

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
  last_footer_block         u32
  checkpoint_generation     u64
  next_checkpoint_slot      green | blue
  log_bytes_since_checkpoint u64
```

No LBA is stored in the mapping because its array index is the LBA. With one serialized writer, the next append block is always `last_footer_block + 1` and is not stored separately.

### Checkpoints

Green and blue are fixed checkpoint slots used alternately. Their 4 KiB descriptors live at physical blocks zero and one; their body regions are reserved at format time.

A descriptor contains:

- magic, format version, slot ID and flags;
- volume ID, logical block size, volume and backing block counts;
- monotonically increasing checkpoint generation;
- checkpoint LSN and last footer block;
- physical-map and checksum-map body locations and lengths;
- checksum algorithm ID, body checksum and descriptor checksum;
- zero-filled padding to complete the 4 KiB descriptor.

The body is an exact snapshot of `physical_blocks` followed by `checksums`, with each array rounded independently to 4 KiB. An `empty_mapping` descriptor flag permits initial checkpoints without writing an all-zero body.

A checkpoint is created synchronously in V0:

1. Stop new writes and drain the current record; choose LSN `S`.
2. Flush the log so every record through `S` is durable.
3. Write the inactive slot's mapping body and flush it.
4. Write and flush its descriptor with the new generation, `S`, footer position and body checksum.
5. Mark it active, reset `log_bytes_since_checkpoint`, and resume writes.

A descriptor with a newer generation is only a candidate: startup must validate its complete body before selecting it. If it is invalid, recovery tries the older slot.

Checkpointing is triggered when physical log bytes written since the last checkpoint reach a configured bound. This directly bounds normal tail-recovery work. V0 performs the checkpoint before accepting another write. `close` always flushes and checkpoints.

### Operations

`format(backing, volume_size)`:

1. Validate 4 KiB alignment, the 8 TiB limit and backing capacity.
2. Compute and reserve both checkpoint body regions and the log start.
3. Allocate the two mapping arrays from `volume_size`.
4. Write two valid empty checkpoint descriptors with LSN zero and `last_footer_block = log_start_block - 1`, then flush them.

There is no zero-payload format record. The two checkpoint descriptors are the format roots and the log starts empty.

`open(backing)`:

1. Open with direct I/O and initialize `io_uring` and aligned buffers.
2. Recover a checkpoint and its log tail as described below.
3. Expose the volume only after recovery completes.

`write_block(lba, data[4096])`:

1. Validate the LBA and reserve the next payload and footer blocks plus the next LSN.
2. Compute the payload checksum and encode the complete footer.
3. Submit the payload and footer as one `io_uring` `WRITEV` operation.
4. Require an exact-length completion, then update both mapping arrays and the log cursors and complete the write.
5. On an error or short write, fail-stop without publishing the new mapping; recovery resolves any partial tail.

`read_block(lba, data[4096])`:

1. Validate the LBA and read its physical block and checksum from the arrays.
2. Return zeros if the physical block is zero.
3. Otherwise submit the 4 KiB read, recompute its checksum and compare it.
4. Return the data or a checksum-mismatch error. V0 detects but cannot repair corruption.

`flush()`:

1. Drain all submitted payload and footer writes.
2. Submit an `io_uring` fsync against the backing descriptor.
3. After successful completion, advance `durable_lsn` and complete the flush.

`close()` performs `flush()`, writes a checkpoint, then closes the ring and backing descriptor. A checkpoint failure makes close fail.

### Crash recovery

Startup reads both checkpoint descriptors. It considers candidates in descending generation order, reads the selected body directly into the mapping arrays, and verifies the complete body checksum. If neither format root is valid, V0 refuses to open; full-log salvage is a separate future tool.

Tail recovery starts at `last_footer_block + 1`, with the checkpoint's LSN and footer position as the expected chain:

1. Probe each 4 KiB-aligned candidate footer position up to the 338-block payload maximum.
2. Derive payload count from the candidate and previous footer positions. Accept only a footer whose metadata checksum, volume, LSN, self-position, previous-footer link and LBA bounds all match expectations.
3. Apply each LBA's derived payload position and checksum to the arrays, then continue after that footer.
4. If no valid footer exists within one maximum record, stop. Records after this gap are ignored.
5. Zero and flush one maximum-record window from the recovered append position so a stale footer cannot reappear, then serve requests.

Recovery does not read or verify payload data. A valid footer with corrupted payload is accepted into the mapping; corruption is detected if that LBA is later read.

### V0 fault model

V0 assumes one writer and a backing device where a successful fsync makes all prior writes durable. It covers:

- incomplete or torn records: an invalid footer truncates the tail; a valid footer with damaged payload is accepted and detected lazily when that LBA is read;
- corrupted footer/checkpoint metadata: checksum failure, tail truncation or fallback to the other checkpoint;
- corrupted payload writes or later bit flips: detected lazily by `read_block` and returned as an error;
- reported backing I/O errors: propagated without publishing the affected mapping.

V0 does not repair corruption, protect against maliciously forged footer data, compact or wrap the log, retry failed writes, support discard/write-zeroes, or provide concurrent writes. The first milestone tests daemon/process crashes; deterministic torn-write and corruption injection comes next.

### Machine-verifiable V0 milestones

Every milestone adds tests to the same cumulative gate:

```sh
zig build test
```

A milestone is complete only when that command exits successfully without weakening prior tests. Data structures are implementation work within a behavioral milestone, not milestones by themselves.

- **v0.0 — build and I/O gate:** Pin Zig 0.16.0, establish the edit-compile-test loop, and perform an aligned `io_uring` write, fsync, reopen and read/compare against the real backing path. Switch to Rust before v0.1 if this path is not small and trustworthy.
- **v0.1 — format:** Format a temporary image. Tests independently verify both descriptors and their checksums, calculated regions, capacity rejection and an empty log.
- **v0.2 — open:** Format, close and open an image. Tests verify the reconstructed `Volume`, zero mapping and checkpoint selection.
- **v0.3 — bootstrap, write and shutdown:** Format, open, write one block, flush and close. Tests inspect the raw payload/footer and resulting checkpoint mapping without using `read_block`.
- **v0.4 — read:** Unwritten LBAs return zeros; written data reads correctly before and after reopen.
- **v0.5 — update semantics:** Write multiple LBAs and overwrite one LBA. Tests prove the latest value wins after a clean reopen.
- **v0.6 — crash recovery:** A subprocess writes after a checkpoint and exits without close. Its parent reopens the image, replays the tail and verifies every block.
- **v0.7 — A/B checkpoints:** Force multiple checkpoints, corrupt the newest descriptor or body, and prove open falls back to the older checkpoint and replays its tail.
- **v0.8 — integrity pass:** Corrupt payload, footer and checkpoint data. Tests cover checksum errors, tail truncation, invalid ranges, short I/O and failed I/O without mapping publication.
- **v0.9 — V0 alpha:** Run a deterministic black-box workload against a byte-array reference model, including writes, overwrites, reads, flushes, clean restarts and hard crashes.

Checksums required to interpret persistent data are implemented with the first relevant milestone. v0.8 expands corruption coverage and assertions rather than retrofitting the format.

### Immediately after V0

1. **v0.10 — basic ublk:** Expose 4 KiB READ, WRITE and FLUSH requests. Run applicable `blktests` ublk coverage—especially mounting and daemon recovery—plus direct fio verification.
2. **v0.11 — filesystem goal:** Create and mount a filesystem, write and fsync files, unmount, restart the daemon, remount and verify hashes.
3. Only then add deterministic fault simulation and broader incomplete, reordered and corrupted I/O coverage. Evaluate `dm-flakey`, `dm-log-writes`, `null_blk` and fio before writing bespoke tooling.
4. Batching, queue-depth tuning, throughput work and AI-harness-driven workload generation come after correctness coverage through the mounted stack.

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
