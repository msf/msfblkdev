# Known bugs and TODOs

Authored by LLM from a design session on 2026-11-25; findings reviewed by human in that session.
GitHub issues are disabled on `msf/msfblkdev`, so we track them here.

Each entry names the code sites, what is verified and what is inferred, and a proposed fix.

## BUG-1 The engine and the daemon assume a regular-file backing

Found 2026-11-25. Status: open.

ADR-08 ([When may the writer reuse a victim?](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#when-may-the-writer-reuse-a-victim))
specifies hole punching for file backing and `BLKDISCARD` for device backing. The implementation
handles files only:

| Site | Problem on a block device |
|---|---|
| `rust/src/format_v4/reclaim/scanner.rs:183-190` (`punch_hole`) | Backing discard always issues `fallocate(PUNCH_HOLE \| KEEP_SIZE)`. On a block device, Linux turns this into `blkdev_issue_zeroout(..., BLKDEV_ZERO_NOFALLBACK)`: a write-zeroes request, not a discard. It fails with `EOPNOTSUPP` when the device has no write-zeroes support. Where it works, it asks for a zeroing guarantee we don't need. Source: `blkdev_fallocate` in `block/fops.c`, Linux master `5f5be658`, read 2026-11-25. |
| `rust/src/format_v4/volume/reclaim.rs:408-421` | The first failed discard disables backing discard for the life of the process. `EOPNOTSUPP` isn't logged, so a device backing loses discard silently. |
| `rust/src/format_v4/volume.rs:265-293` (`space_usage`) | `metadata().len()` and `metadata().blocks()` don't describe a device. `backing_bytes` and `allocated_bytes` are wrong there. |
| `rust/src/format_v4/volume/growth.rs:7-9` (`backing_len`) | Returns 0 for a device. `grow_backing` itself uses `seek(End)`, which works. |
| `rust/src/bin/block-storage-ublk.rs:1467-1520` (`format_new`) | Creates the backing with `create_new(true)` and sizes it with `set_len`. Every device path already exists, so this fails with `EEXIST`: the daemon can't format a device. On a later failure it calls `remove_file` on the path; a device variant must not inherit that cleanup. |

What is correct today: on a regular file on XFS, ext4, Btrfs or tmpfs, `PUNCH_HOLE | KEEP_SIZE`
deallocates the range and later reads return zeroes (fallocate(2), man7.org, read 2026-11-25). That
is the right primitive for files, and `BLKDISCARD` doesn't apply to them. Whether the host
filesystem then discards the freed blocks on its SSD depends on its `discard` mount option or a
periodic `fstrim`.

Size discovery already works on both kinds: `open_backing` (`rust/src/format_v4/storage.rs:31-42`)
uses `seek(SeekFrom::End(0))`.

Proposed fix: classify the backing once when it is opened (`fstat`: `S_ISREG` or `S_ISBLK`) and
dispatch on that kind:

- regular file: size from `seek(End)`, release with `PUNCH_HOLE | KEEP_SIZE`, allocation from `st_blocks`;
- block device: size from `seek(End)`, release with `BLKDISCARD` over the exact segment byte range, no allocation figure;
- both: log every reason that disables backing discard;
- format: format an existing device in place, with no `create_new`, `set_len` or `remove_file`.

Related, not yet a bug: segments start at block 2, so they sit 8 KiB off natural device alignment
(ADR-08 [Persistent layout](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#persistent-layout)). A
device with a `discard_granularity` above 4 KiB may ignore the unaligned head and tail of each
segment discard. Measure on a real device before changing the layout.

## BUG-2 Nothing protects a backing shared by several hosts

Found 2026-11-25. Status: open; out of scope until the
[distributed design](DISTRIBUTED_RELIABLE_BLOCK_STORAGE.md) needs shared backing.

On one host, `open_live()` claims the backing before recovery: `flock(LOCK_EX | LOCK_NB)` on a
regular file and `O_EXCL` on a block device (ADR-11). Neither claim reaches another host.

An on-disk lock with an expiration LSN doesn't work by itself. The LSN advances only when the owner
writes, so an idle owner and a dead owner look the same. A lease needs wall-clock heartbeat writes
(ZFS `multihost` does this with periodic uberblock writes and an import delay; from memory, not
re-read). Even then, an owner that pauses past its lease can resume and write. Only fencing at the
device, such as NVMe or SCSI persistent reservations, stops a stale owner.

## TODO-1 Make the engine deterministic for a given seed

Found 2026-11-25. Status: open.

Goal: given a fixed volume ID, request sequence and clock trace, the engine produces a
byte-identical backing image, in live runs and in tests. ADR-05 Delivery 1 already requires
byte-identical formatted images for a fixed volume ID.

Candidate sources, found by grep and not yet analysed for effect:

| Source | Sites |
|---|---|
| `std::hash::RandomState`, seeded per process. It matters only where iteration order reaches a decision or the disk. | mapping cache lookup (`rust/src/mapping_cache/sizing.rs:5`, `89`); write buffer `slot_of_lba` (`rust/src/lib.rs:272`, `302-303`) |
| Wall-clock maintenance. Idle and interval deadlines decide when cleaning and checkpoints run. | daemon loop (`rust/src/bin/block-storage-ublk.rs:1154-1227`); `rust/src/format_v4/reclaim/scheduler.rs` (already takes an injectable clock); `rust/src/format_v4/volume/reclaim.rs:215` |
| The cleaner thread. Proposals reach the writer at times set by thread scheduling. | `rust/src/format_v4/reclaim/cleaner.rs` |
| The volume ID comes from `/dev/urandom`. It is the intended seed, but format has no way to set it. | `rust/src/lib.rs:410-414`, `rust/src/format_v4/storage.rs:75` |

Steps: list every source; seed hashers and any random choice from the volume ID; make time an input
to the engine; make the points where cleaner results reach the writer deterministic in a test mode;
let format take an explicit volume ID.

## TODO-2 Checkpoint load reads one block at a time

Found 2026-11-26. Status: open.

Startup reads every page that the selected root references, one block per round trip. Each read
goes through `read_block_exact` and `submit_exact`, which submit one request and wait for it before
the next. This covers the indirect and direct index pages and the segment table pages
(`rust/src/format_v4/checkpoint_load.rs:105-175`) and every mapping page in `validate_snapshots`
(`checkpoint_load.rs:252-286`). The engine's ring has 2 entries (`rust/src/accounted_io.rs:18`).

A fully mapped 500 GiB volume has 390,096 mapping pages, so startup waits for about 390,000
sequential reads. We estimate tens of seconds on NVMe; not measured.

Proposed fix: run the same pipeline in batches of `N` pages:

1. Take the next `N` page IDs.
2. Look up their physical blocks in the mapping index and skip zero pointers.
3. Sort the batch by physical block.
4. Submit `N` reads at once and wait for all of them.
5. Decode and validate each page, then add its entries to the live counts.

The counters only add, so completion order doesn't matter. Index pages load first, in the same
batched way, because mapping page pointers come from them. The ring needs at least `N` entries,
and the loader needs `N` aligned block buffers.

`open_readonly()` (ADR-11) needs the same walk; `open_live()` should reuse it.

## Checked, not a bug: on-disk format version

Checked 2026-11-25. Every metadata block already carries the format version, and the code rejects
any other version before it trusts a field:

- The checkpoint descriptor stores magic `VBLC` and version byte 4 at offset 4
  (`rust/src/format_v4/descriptor.rs:116-120`).
- Every record header stores magic `VBLF` and version byte 4 at offset 4
  (`rust/src/format_v4.rs:184-192`).
- `validate_block_checksum` rejects a magic or version mismatch (`rust/src/format_v4.rs:224-238`).
  `decode_fields` and the tail probe reject unknown versions and kinds
  (`rust/src/format_v4.rs:195-206`, `220-222`).
- Payload blocks have no header, by design. Their data-record footer carries the version.

The remaining gap is that the version is one byte with no compatible or incompatible feature flags.
ADR-09 leaves that decision open until compressed records need it.
