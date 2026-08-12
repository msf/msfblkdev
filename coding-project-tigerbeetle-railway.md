# Coding project for TigerBeetle and Railway

## Project thesis

This project builds a userspace, log-structured virtual block device. It initially uses one local backing store. The design supports deterministic fault testing and can later support quorum replication.

Linux exposes the device locally (for example, `/dev/my-vol`) through ublk, its framework for userspace block devices. A local filesystem such as ext4 or XFS can mount the device. An existing database such as PostgreSQL or a key-value store can then run unchanged on it.

```text
Database / key-value store
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
3. A log structure that makes later replication and high availability easier to reason about.
4. Sequential physical writes and random reads, appropriate for NVMe-backed storage.
5. A future distributed form as a replicated state machine/log, including quorum commit and fencing.

## Language and engineering style

- **Zig 0.16.0 is pinned.** The project-local compiler is downloaded from ziglang.org and excluded from Git.
- Zig 0.16's high-level `std.Io.Uring` backend is explicitly unfinished. Before using Zig beyond v0.0, implement a bounded spike with the low-level `std.os.linux.IoUring`. Open a direct-I/O backing store. Perform aligned read, write and fsync operations. Validate completion queue entry (CQE) errors and short I/O. Then reopen the backing store and verify the data.
- If the direct `io_uring` path is not small and trustworthy, use Rust with the low-level `io-uring` crate. I/O ergonomics are a valid language-selection criterion. Do not add an async runtime.
- Do not use Go. This project requires explicit control of memory, alignment and low-level I/O lifetimes.
- Follow TigerStyle where applicable. Use simple control flow, bounded resources, assertions, checksums and an explicit fault model. Apply the principles instead of copying constraints without context.

## Scope boundary

The initial project is local and single-node. Remote transport, replication, multiple backing stores, live migration, compaction and performance optimization are possible extensions. They are not current commitments.

The backing store can theoretically be anything that satisfies the block contract. A local file or raw block device keeps the project focused. The important artifact is the storage engine and its correctness evidence, not an exotic backend.

## V0 target design

This section describes the target design through v0.9. The [milestone section](#machine-verifiable-v0-milestones) defines which behavior each intermediate version must provide.

V0 is the standalone local storage engine before ublk integration. It uses a pre-sized file or raw block device through `io_uring`. It supports one serialized writer and has no compaction or log wraparound. The caller creates and pre-sizes a regular backing file. The formatter formats an existing file or raw block device in place.

### Bounds and layout

- Logical block size: 4 KiB.
- Maximum virtual volume: 8 TiB = `2^31` logical blocks. Logical block address (LBA) IDs and the volume block count use `u32`. An LBA ID must have its high bit zero. The volume block count can equal `2^31`. Validate a logical range with `start <= volume_blocks` and `count <= volume_blocks - start`. Byte sizes, byte offsets, ublk's 512-byte sector addresses and local sequence numbers (LSNs) use `u64`.
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
RAM mapping                  N × 12 bytes
physical-map bytes           ceil(N × 4 / 4096) × 4096
checksum-map bytes           ceil(N × 8 / 4096) × 4096
one checkpoint body          physical-map bytes + checksum-map bytes
both checkpoint bodies       2 × one checkpoint body
```

The allocation follows the configured volume size, not the 8 TiB format maximum. At 4 TiB the mapping uses 12 GiB of RAM; at 8 TiB it uses 24 GiB. The backing store must also have room for both checkpoint bodies and the finite V0 log.

### Checksums

V0 uses XXH3-64 checksums. The persistent format version defines the checksum algorithm and its inputs. These checksums detect accidental corruption. They do not authenticate data from a malicious block client.

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
| 6 | 2 | zero padding |
| 8 | 8 | volume ID |
| 16 | 8 | local sequence number |
| 24 | 4 | previous footer physical block |
| 28 | 4 | this footer's expected physical block |
| 32 | 1352 | `lba_ids[338]`, each `u32` |
| 1384 | 2704 | `checksums[338]`, each `u64` |
| 4088 | 8 | footer checksum |

The footer bounds a record to 338 payload blocks. This is 1.3203125 MiB of payload and 339 physical blocks including the footer. The payload count is `footer_block - previous_footer_block - 1`. The initial checkpoint uses `log_start_block - 1` as the previous-footer boundary sentinel. LBA IDs must be unique within a record. V0 writes one block per record. The format can support batching later without changing recovery.

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
  log_bytes_since_checkpoint u64    # added with bounded recovery in v0.6
```

No LBA is stored in the mapping because its array index is the LBA. With one serialized writer, the next append block is always `last_footer_block + 1` and is not stored separately.

### Checkpoints

Green and blue are fixed checkpoint slots used alternately. Their 4 KiB descriptors live at physical blocks zero and one; their body regions are reserved at format time.

A descriptor contains only the state required to identify and recover a checkpoint:

- magic, format version and slot ID;
- volume identity and geometry;
- checkpoint LSN and last footer block;
- body checksum and descriptor checksum;
- zero-filled padding to complete the 4 KiB descriptor.

The slot and volume geometry determine the checkpoint-body location and length. The format version determines the checksum algorithm.

The body is an exact snapshot of `physical_blocks` followed by `checksums`. Each array is rounded independently to 4 KiB. An initial descriptor has checkpoint LSN zero and `last_footer_block = log_start_block - 1`. This state implies an empty mapping, so format does not need to write an all-zero checkpoint body. Startup does not read the body for this initial state.

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

Through v0.5, `close` is the only required checkpoint trigger. If `last_lsn == checkpoint_lsn`, the mapping has not changed and `close` does not write another checkpoint. V0.6 adds a configured bound on physical log bytes written since the last checkpoint. Before accepting a write that exceeds this bound, V0 creates a checkpoint. The v0.6 design must define how callers configure the bound.

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

On an error or short write, the volume enters a failed state without publishing the new mapping. No read, write or flush operation is valid in this state. The caller can only release the volume. A later `open` resolves or rejects any partial tail according to the implemented recovery milestone.

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

The following tail-recovery algorithm is the v0.6 target. Earlier milestones reject a valid unreplayed tail.

Tail recovery starts at `last_footer_block + 1`. The selected checkpoint supplies the expected LSN and footer position:

1. Probe each 4 KiB-aligned candidate footer position up to the 338-block payload maximum.
2. Derive payload count from the candidate and previous footer positions. Accept only a footer whose metadata checksum, volume, LSN, self-position, previous-footer link and LBA bounds all match expectations.
3. Apply each LBA's derived payload position and checksum to the arrays, then continue after that footer.
4. If no valid footer exists within one maximum record, stop. Records after this gap are ignored.
5. Zero and flush one maximum-record window from the recovered append position so a stale footer cannot reappear, then serve requests.

Recovery does not read or verify payload data. A valid footer with corrupted payload is accepted into the mapping; corruption is detected if that LBA is later read.

### V0 fault model

V0 assumes one writer and a backing store where a successful fsync makes all prior writes durable. It covers:

- incomplete or torn records: an invalid footer truncates the tail; a valid footer with damaged payload is accepted and detected lazily when that LBA is read;
- corrupted footer/checkpoint metadata: checksum failure, tail truncation or fallback to the other checkpoint;
- corrupted payload writes or later bit flips: detected lazily by `read_block` and returned as an error;
- reported backing I/O errors: propagated without publishing the affected mapping.

V0 does not repair corruption, protect against maliciously forged footer data, compact or wrap the log, retry failed writes, support discard or write-zeroes, or provide concurrent writes. Milestone v0.6 tests process crashes. Milestone v0.8 adds deterministic torn-write and corruption injection.

### Machine-verifiable V0 milestones

Every milestone adds tests to its implementation's cumulative gate:

```sh
# Zig
zig build test

# Rust (from rust/)
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

A milestone is complete only when its gate exits successfully without weakening prior tests. Data structures are implementation work within a behavioral milestone, not milestones by themselves.

Recovery requirements grow with the milestones:

| milestone | required recovery behavior |
|---|---|
| v0.0–v0.1 | No recovery requirement. |
| v0.2–v0.5 | Open a clean checkpoint. Reject a valid unreplayed tail. |
| v0.6 | Replay a valid tail after a process crash. |
| v0.7 | Fall back from an invalid newer checkpoint and replay from the older checkpoint. |
| v0.8 | Detect the specified injected corruption and torn-I/O cases. |
| v0.9 | Preserve behavior under the deterministic model-based workload. |

- **v0.0 — build and I/O gate:** Pin Zig 0.16.0, establish the edit-compile-test loop, and perform an aligned `io_uring` write, fsync, reopen and read/compare against the real backing store. Switch to Rust before v0.1 if this path is not small and trustworthy.
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
