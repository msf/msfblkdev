# ADR-04 Growable thin-provisioned format

Date: 2026-08-29
Author: Miguel Filipe
Status: proposed
On-disk format: 2
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md), [ADR-05](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md)
Would update: ADR-01 persistent-format sections when accepted and implemented

## Context

ADR-03 validates crash recovery for the fixed-size V0 format. That work exposed three limits which are outside ADR-03's scope:

1. Checkpoint-body locations depend on the current logical volume size. Growing the volume would move both bodies and the log start.
2. Dense checkpoint maps allocate metadata for every logical block, including blocks which were never written. At the 8 TiB logical maximum, one checkpoint body is 24 GiB and two fixed bodies are 48 GiB.
3. If both checkpoint roots are unusable, the log does not contain enough immutable geometry to reconstruct the original volume safely.

ADR-03 keeps its existing acceptance contract. Normal V0.6 startup uses one valid checkpoint root and fails closed if both roots are unusable. This ADR records the follow-up architecture triggered by that validation. It is not another ADR-03 delivery and does not change the ADR-01 delivery order.

The target is a thin-provisioned device which can:

- start with a 128 MiB logical size and backing store;
- grow its logical address space up to the format maximum;
- grow its assured physical backing independently;
- keep one bottom-up data-log append head;
- relocate its checkpoint storage without changing logical data;
- compact into a new crash-safe image;
- preserve blue/green checkpoint fallback during every transition.

## Decision

The next persistent format will use three fixed 4 KiB blocks followed by one forward record stream:

```text
physical block 0      root pointer A
physical block 1      root pointer B
physical block 2      immutable FORMAT record, record LSN zero
physical block 3...   immutable records and reserved checkpoint arenas
```

The two fixed roots point directly to blue and green checkpoint slots. Each checkpoint arena reserves two mutable slots at an offset in the forward stream. Normal checkpoints update those slots in place. When the current slots are too small, the engine appends a larger arena, initializes both slots, and redirects the fixed roots one at a time.

Logical growth, backing provisioning and checkpoint-arena allocation are separate state transitions:

- `GROW_LOGICAL` changes the user-visible logical block count.
- `PROVISION_BACKING` increases the physical block range the engine may use.
- `CHECKPOINT_ARENA` reserves larger blue/green checkpoint slots.

A logical grow does not allocate a dense map and does not necessarily relocate the checkpoint arena. The engine stores only mapped logical block addresses (LBAs). A checkpoint arena grows when its mapping-entry capacity must grow.

The V0 milestone family uses on-disk format version 1. This ADR defines on-disk format version 2, abbreviated as V2 below. Existing format-version-1 images remain governed by ADR-01 and ADR-03. This ADR does not require in-place compatibility.

## Capacity model

The format keeps these capacities distinct:

```text
max_logical_blocks           Immutable format limit. At most 2^31 blocks (8 TiB).
logical_blocks               Current user-visible device size.
actual_backing_blocks        Current size reported by the file or block device.
provisioned_backing_blocks   Durable physical-block count the engine may use.
checkpoint_entry_capacity    Maximum mapped LBAs each current checkpoint slot can encode.
append_block                 Next block in the immutable record stream.
mapped_entries               Current number of mapped LBAs.
```

The invariants are:

```text
128 MiB / 4096 <= logical_blocks <= max_logical_blocks <= 2^31
append_block <= provisioned_backing_blocks <= actual_backing_blocks
provisioned_backing_blocks <= 2^32 - 1
mapped_entries <= logical_blocks
mapped_entries <= checkpoint_entry_capacity
```

V2 encodes `append_block` and physical block addresses as `u32`. The provisioned count is therefore at most `u32::MAX` blocks, so the one-past-last append position remains representable. The actual backing store may be larger; the engine ignores blocks beyond the provisioned bound. An external expansion does not become usable until a durable `PROVISION_BACKING` record claims it. Startup fails if the actual backing store is smaller than the recovered provisioned bound.

There is no persistent estimate of usable user bytes. The engine accounts in exact physical blocks. A V2 WRITE record with `N` payload blocks consumes `N + 1` physical blocks, so its payload-write amplification is `(N + 1) / N`. One-block records are exactly 2x. A 338-block batch is approximately 1.003x. Metadata and checkpoint writes are counted separately.

The control plane may maintain a physical-space watermark and grow the backing store before the append head reaches it. The format records the provisioned end, not the watermark policy.

## Common encoding rules

All persistent structures are 4 KiB aligned and explicitly encoded little-endian. Physical block addresses, record-header addresses and append positions use `u32`. Block counts and byte arithmetic use `u64`. Every complete structure ends at a 4 KiB boundary.

The on-disk format version is `2`. Readers reject an unknown format version or record kind before interpreting kind-specific fields. Unknown state transitions are not skippable.

The proposed [fault model and failure policy](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md#fault-model-and-failure-policy) lives in ADR-05. Its checksum-chain and tail-reuse proposals remain unresolved; this ADR does not yet encode them. V2 keeps XXH3-64 checksums. Each root, record header and checkpoint descriptor uses unseeded XXH3-64 over its complete 4 KiB block with the checksum field set to zero. Unused bytes inside a checksummed block must be zero. These checksums are not authentication.

## Fixed root pointers

Blocks zero and one contain independent root pointers. A valid root identifies one checkpoint slot in one checkpoint arena.

| offset | size | field |
|---:|---:|---|
| 0 | 4 | magic (`VBLR`) |
| 4 | 1 | format version (`2`) |
| 5 | 1 | root slot (`A = 0`, `B = 1`) |
| 6 | 2 | zero |
| 8 | 8 | volume ID |
| 16 | 8 | checkpoint-arena generation |
| 24 | 4 | checkpoint-arena header block |
| 28 | 4 | checkpoint descriptor block |
| 32 | 4056 | zero |
| 4088 | 8 | root checksum |

Root A always points to the green slot. Root B always points to the blue slot. Arena generation starts at one and increments on every relocation.

Root bootstrap uses the actual backing size because the provisioned bound is inside the checkpoint being located. All pointer and extent arithmetic is checked before I/O. After reading the checkpoint, startup requires its provisioned bound to be no larger than the actual backing and to contain the complete referenced arena.

A root is usable only when:

- its checksum, magic, version, root slot and zero padding are valid;
- its volume ID matches the valid FORMAT record;
- its pointers are within the actual backing store;
- the referenced arena header has the same volume ID and arena generation;
- the referenced descriptor belongs to the root's required green or blue slot;
- the checkpoint's provisioned bound contains the complete arena;
- the complete checkpoint body is valid.

Startup evaluates each root independently. It selects the usable checkpoint with the highest checkpoint local sequence number (LSN). Equal-LSN roots must encode equivalent logical size, provisioned bound and mapping state; startup rejects a conflict.

A normal checkpoint does not update blocks zero or one. Root pointers change only when checkpoint slots relocate.

## FORMAT is record zero

Physical block two contains the immutable `FORMAT` record header. It is record LSN zero and is the fixed starting point for future offline salvage.

V2 assigns these record-kind values:

```text
FORMAT = 1
WRITE = 2
GROW_LOGICAL = 3
PROVISION_BACKING = 4
CHECKPOINT_ARENA = 5
```

Every immutable record starts with this common header:

| offset | size | field |
|---:|---:|---|
| 0 | 4 | magic (`VBLG`) |
| 4 | 1 | format version (`2`) |
| 5 | 1 | record kind |
| 6 | 2 | payload block count |
| 8 | 8 | volume ID |
| 16 | 8 | record LSN |
| 24 | 4 | previous record-header block |
| 28 | 4 | this record's expected header block |
| 32 | 4056 | kind-specific fields followed by zero padding |
| 4088 | 8 | record-header checksum |

FORMAT, `GROW_LOGICAL`, `PROVISION_BACKING` and `CHECKPOINT_ARENA` have zero payload blocks. A WRITE has 1 to 338 payload blocks immediately after its header. This header-first framing gives every record one unambiguous start without adding another physical block to WRITE.

The FORMAT fields are:

| offset | size | field |
|---:|---:|---|
| 32 | 4 | logical block size (`4096`) |
| 36 | 4 | initial logical blocks |
| 40 | 4 | maximum logical blocks |
| 44 | 4 | zero |
| 48 | 8 | initial provisioned backing blocks |

For FORMAT:

```text
record_lsn = 0
previous_record_header = 1
self_record_header = 2
payload_blocks = 0
```

Formatting writes FORMAT at LSN zero and the initial `CHECKPOINT_ARENA` at LSN one. It writes and fsyncs both initial checkpoint slots before writing either fixed root. It then writes and fsyncs root A followed by root B. A durable root can therefore never point to an undurable initial arena. The initial arena generation is one. Both initial checkpoint slots include the arena record and represent the same empty mapping and the same initial logical and provisioned sizes.

## Immutable record stream

Every immutable record increments the record LSN. Mutable checkpoint-slot writes do not increment it.

The next record normally starts at:

```text
record_header_block + 1 + payload_blocks
```

A `CHECKPOINT_ARENA` header instead supplies the block after its reserved mutable extent. Recovery reads exactly one header at each expected record start and classifies it under the referenced failure policy.

A WRITE header uses the common prefix followed by fixed arrays:

| offset | size | field |
|---:|---:|---|
| 32 | 1352 | `lba_ids[338]`, each `u32` |
| 1384 | 2704 | `checksums[338]`, each `u64` |

Only the first `payload_blocks` entries are used. Remaining entries must be zero. Payload entry `i` describes physical block `record_header_block + 1 + i`. A maximum-sized WRITE occupies 339 physical blocks including its header.

WRITE payload checksums use the ADR-01 formula. The record-header checksum uses unseeded XXH3-64 over the complete header with its checksum field set to zero. Recovery validates the header checksum, volume ID, LSN, previous-record link, self-position, payload range, current logical-size bound, LBA uniqueness and unused zero entries before applying it.

An exact completed write publishes the mapping. Header-first framing does not change the distinction between metadata and payload validation. Payload validation and online read failures follow the proposed ADR-05 policy.

## Sparse checkpoint mapping

A checkpoint body is a sorted array of 16-byte mapping entries:

| offset | size | field |
|---:|---:|---|
| 0 | 4 | LBA |
| 4 | 4 | physical payload block |
| 8 | 8 | payload checksum |

Entries are strictly ordered by LBA. An LBA appears at most once. Unmapped LBAs are absent and read as zeroes. A mapped zero payload remains an ordinary mapped entry until the format adds discard semantics.

Each 4 KiB body block stores 256 entries. The final used body block is zero-padded. Checkpoint validation rejects:

- an entry count larger than the arena capacity;
- duplicate or unordered LBAs;
- an LBA outside the checkpoint's logical size;
- a physical payload block outside the provisioned range;
- a physical payload block at or after the checkpoint's append block;
- non-zero final-block padding;
- a body checksum mismatch.

Normal checkpoint recovery does not reread one WRITE header per mapping entry. The complete body checksum protects the snapshot under V2's accidental-corruption model, and `read_block` still verifies that the referenced physical bytes match the LBA-bound payload checksum. Offline salvage validates every WRITE header and payload before rebuilding a checkpoint.

The initial implementation may use a standard in-memory sparse map and sort its entries when checkpointing. A paged or copy-on-write map is deferred until measurements show that full sparse snapshots dominate checkpoint latency.

## Checkpoint arenas

A `CHECKPOINT_ARENA` record header at physical block `H` reserves two equal mutable slots immediately after itself:

```text
H                              immutable CHECKPOINT_ARENA header
H + 1                          green checkpoint descriptor
green descriptor + 1          green body capacity
blue descriptor               blue checkpoint descriptor
blue descriptor + 1           blue body capacity
next_record_header             next immutable record
```

Its kind-specific fields are:

| offset | size | field |
|---:|---:|---|
| 32 | 4 | next record-header block |
| 36 | 4 | mapping-entry capacity per slot |
| 40 | 4 | physical blocks per slot, including descriptor |
| 44 | 4 | green descriptor block |
| 48 | 4 | blue descriptor block |
| 52 | 4 | zero |
| 56 | 8 | arena generation |

The arena generation starts at one and increments by one on relocation. The locations must satisfy:

```text
body_capacity_blocks = ceil(checkpoint_entry_capacity / 256)
slot_blocks = 1 + body_capacity_blocks
green_descriptor_block = H + 1
blue_descriptor_block = H + 1 + slot_blocks
next_record_header = H + 1 + 2 * slot_blocks
```

The complete arena must fit within the provisioned backing range. Slot capacity may conservatively follow the number of physical blocks provisioned for the volume. A 128 MiB backing store has 32,768 physical blocks; reserving one 16-byte sparse entry per physical block requires at most 512 KiB per checkpoint body and approximately 1 MiB for both slots.

The arena header is immutable. The two slots are mutable reserved extents, not immutable records. Recovery skips them through the header's `next_record_header`. Only the used checkpoint body blocks are written and checksummed. Bytes after the descriptor's body length are outside that checkpoint and need not be zeroed on every update.

## Checkpoint descriptor

The first block in each arena slot is its checkpoint descriptor:

| offset | size | field |
|---:|---:|---|
| 0 | 4 | magic (`VBLC`) |
| 4 | 1 | format version (`2`) |
| 5 | 1 | checkpoint slot (`green = 0`, `blue = 1`) |
| 6 | 2 | zero |
| 8 | 8 | volume ID |
| 16 | 8 | arena generation |
| 24 | 8 | checkpoint LSN |
| 32 | 4 | final record-header block |
| 36 | 4 | recovered append block |
| 40 | 4 | logical blocks |
| 44 | 4 | mapped-entry count |
| 48 | 8 | provisioned backing blocks |
| 56 | 4 | used checkpoint-body blocks |
| 60 | 4 | zero |
| 64 | 8 | checkpoint-body checksum |
| 72 | 4016 | zero |
| 4088 | 8 | descriptor checksum |

The descriptor's append block must match the next-record position derived from its final record header. `used_checkpoint_body_blocks` must equal `ceil(mapped_entries / 256)`, with zero blocks for an empty mapping. The mapped-entry count must fit both that body length and the arena capacity.

The checkpoint-body checksum is zero for an empty mapping. Otherwise it uses unseeded XXH3-64 over the complete sequence of used 4 KiB body blocks, including the final block's zero padding.

## Normal checkpoint publication

The engine alternates between green and blue slots in the current arena:

1. Stop new writes and flush the immutable record stream through LSN `S`.
2. Encode the current sorted mapping into the inactive slot's body.
3. Write the used body blocks.
4. Fsync the backing store.
5. Write the inactive slot's descriptor with LSN `S`, current sizes and body checksum.
6. Fsync the backing store.
7. Select that slot in memory and resume writes.

A crash before the descriptor fsync leaves the previously active slot usable. After the descriptor fsync, startup selects the new higher-LSN checkpoint. Replaying from either valid slot produces the same durable logical state.

A full sparse checkpoint still consumes write bandwidth proportional to the number of mapped LBAs. In-place slots prevent each checkpoint from permanently consuming append space; they do not remove checkpoint write latency.

## Logical growth

`GROW_LOGICAL` is a metadata-only immutable record containing:

| offset | size | field |
|---:|---:|---|
| 32 | 4 | previous logical blocks |
| 36 | 4 | new logical blocks |

The transition is valid only when:

```text
current_logical_blocks < new_logical_blocks <= max_logical_blocks
```

Shrink is not part of V2. Before completing a logical grow, the engine:

1. Ensures its in-memory sparse mapping can represent the new range without allocating entries for unwritten LBAs.
2. Appends the exact `GROW_LOGICAL` record.
3. Fsyncs the record stream.
4. Updates the in-memory logical size.
5. Reports the new size to the caller.

The frontend may expose the new geometry only after step 3 succeeds. Writes to newly valid LBAs cannot precede the durable grow record. Recovery applies the grow before validating later WRITE records against the larger range.

A logical grow does not require a new checkpoint arena unless a separate capacity guarantee requires more mapping entries to be checkpointable.

`GROW_LOGICAL` is idempotent by target size. If recovery already exposes `new_logical_blocks`, a retry succeeds without appending another record. A process loss after the record fsync but before the reply can make the operation succeed durably even when the caller did not receive success; the caller must reread the logical size before retrying.

## Backing provisioning

The storage-node or test harness grows and preallocates the backing medium before asking the engine to claim it. For regular files, changing file length alone does not guarantee allocated extents; deterministic physical allocation requires a supported preallocation operation. Raw-device growth remains an external operation.

`PROVISION_BACKING` contains:

| offset | size | field |
|---:|---:|---|
| 32 | 8 | previous provisioned backing blocks |
| 40 | 8 | new provisioned backing blocks |

The engine:

1. Verifies that the actual backing store has at least the requested number of blocks.
2. Verifies that the `PROVISION_BACKING` header itself fits within the old provisioned range.
3. Appends and fsyncs `PROVISION_BACKING`.
4. Advances the in-memory provisioned bound.

The controller must request growth before the old range is completely full. Capacity watermarks and growth increments are policy, not persistent-format fields.

An older checkpoint recovers the previous bound and then replays the provision record. An unrecorded physical extension remains unused.

`PROVISION_BACKING` is idempotent by target bound. If recovery already exposes `new_provisioned_backing_blocks`, a retry succeeds without appending another record. A process loss after the record fsync but before the reply can make the operation succeed durably even when the caller did not receive success; the caller must reread the provisioned bound before retrying.

## Checkpoint-arena relocation

Before accepting a WRITE, the engine computes how many previously unmapped LBAs the complete batch would add. It relocates the arena first if the resulting mapping would exceed `checkpoint_entry_capacity`. Provisioning may relocate it earlier to avoid a foreground stall.

Relocation uses only the current append head and the two fixed roots:

1. Stop new writes and flush the current record stream.
2. Choose a larger mapping-entry capacity.
3. Verify that the complete new arena fits within the provisioned range.
4. Append the immutable `CHECKPOINT_ARENA` header. Its LSN is the next immutable-record LSN, and its `next_record_header` skips both new slots.
5. Initialize the new green and blue slots with equivalent checkpoints that include the arena record and current state.
6. Fsync the complete new arena.
7. Update and fsync fixed root A to the new green slot.
8. Update and fsync fixed root B to the new blue slot.
9. Move the in-memory append block past the arena and resume writes.

A crash before step 7 leaves both old roots usable. A crash between steps 7 and 8 leaves one new and one old root. The newer valid checkpoint wins; the older root remains a fallback if the new candidate is unusable. A crash after step 8 leaves two equivalent new roots.

Recovery always uses a valid `CHECKPOINT_ARENA` header to skip its reserved extent. It redirects missing roots only when both new slots are valid and equivalent. Otherwise it abandons that arena and continues replay after the extent. This preserves later records if checkpoint-slot corruption forces recovery from an older arena.

The old arena becomes an unreachable reserved extent after both roots move. V2 does not overwrite that range during relocation.

## Recovery

Normal startup remains checkpoint based:

1. Read and validate the fixed FORMAT record against the actual backing size.
2. Read fixed roots A and B.
3. Validate each referenced arena, checkpoint descriptor and complete sparse body independently.
4. Select the newest usable checkpoint.
5. Reconstruct the sparse mapping and all logical, provisioned and append cursors.
6. Replay immutable record headers in LSN order from the checkpoint append block.
7. Apply `GROW_LOGICAL` and `PROVISION_BACKING` before validating later writes against their updated bounds.
8. Validate each `CHECKPOINT_ARENA` header and skip its mutable extent. Treat its slots as relocation candidates, not replay state.
9. Handle replay termination under the proposed ADR-05 policy. The [replacement for recovery-time zeroing](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md#proposal-checksum-chaining-instead-of-recovery-time-zeroing) must be resolved before this recovery algorithm is complete.
10. From the selected and replayed arenas, find the newest arena whose two slots are valid and equivalent. Complete its missing root updates before serving requests.

Normal startup fails closed if neither fixed root reaches a usable checkpoint. That remains the ADR-03 contract.

A future offline salvage tool may start at FORMAT block two and scan every immutable record. It must validate every WRITE payload before rebuilding roots. Arena slots are hints during salvage, not required history; the immutable arena header provides the block at which scanning resumes.

## Compaction

Checkpoint relocation and data compaction share one invariant:

> Never overwrite or reclaim a physical payload while either valid root can select a checkpoint that references it.

Updating checkpoint A before rewriting live payloads from the beginning of the same backing store is not sufficient. Checkpoint A still points at source payloads which the rewrite may overwrite. A crash before checkpoint B becomes durable can therefore destroy the only complete copy of an LBA.

V2's first full compaction operation writes a replacement backing image rather than rewriting the active image in place:

1. Quiesce and flush the source volume.
2. Create a destination with enough provisioned space for FORMAT, an arena, every mapped payload, record headers and planned free space.
3. Write the destination FORMAT record as a new baseline: preserve the volume ID and maximum logical size, use the source's current logical size as the destination's initial size, and record the destination's provisioned bound. Write its checkpoint arena without publishing either fixed root.
4. Read every source mapping in sorted LBA order and verify its payload checksum.
5. Append the validated live payloads as destination WRITE records, batching where supported.
6. Write equivalent final green and blue checkpoints into the destination arena.
7. Fsync the complete destination.
8. Write and fsync both destination root pointers last.
9. Open and verify the destination through normal recovery.
10. Let the external owner atomically replace or reattach the source only after verification.

A crash before step 8 leaves an unpublished destination and an untouched source. A crash during the external switch is handled by that owner's fencing or atomic replacement contract. The engine never treats two backing images as one local append stream.

The destination is compact: its live records start immediately after its initial checkpoint arena, and both roots describe only those records. Old checkpoint arenas, overwritten payloads and stale tails are absent.

Online in-place compaction requires a segmented circular log or scratch space large enough to preserve every source block which might be overwritten. A future compaction ADR may copy live records to the same append head, publish enough new roots to remove all references to a victim segment, and then recycle that segment with a new generation. That design still has one append head, but its physical address eventually wraps. V2 does not define segment reuse.

## Why not rewrite the active image from log start?

The strongest form of the in-place proposal is:

```text
publish checkpoint A
rewrite live LBAs from the first log block
publish checkpoint B
```

This fails if a destination block overlaps a live source block which checkpoint A still references. Arbitrary mappings can contain cycles, so copy order alone cannot make the operation safe without scratch space. Zero padding and checksums detect an interrupted rewrite but cannot recover the overwritten payload.

We therefore choose replacement-image compaction first. It is slower and requires temporary capacity, but its crash argument is short: the source remains authoritative until the destination has two valid roots and passes normal open.

## Compatibility and upgrades

A format-version-2 reader may support format version 1 through an explicit read or copy-conversion path. It must not reinterpret a format-version-1 block as version 2. Format-version-1 readers reject version 2 before serving requests.

A future at-rest protocol transition may add a new record kind and format version. Recording an upgrade boundary does not make old software compatible with new semantics. Software which does not understand a state-changing record must fail closed.

## Consequences

The design provides:

- a fixed three-block bootstrap independent of logical size;
- a 128 MiB starting point without a 48 GiB metadata reservation;
- sparse mappings whose persistent size follows written LBAs;
- crash-safe logical and physical growth;
- reusable blue/green checkpoint storage;
- crash-safe relocation of that storage;
- a simple full-compaction path with an untouched source image;
- enough immutable history for a future offline salvage tool.

We accept these costs:

- checkpoint writes remain proportional to mapped LBAs;
- checkpoint slots are mutable exceptions to the immutable record stream;
- old arenas consume physical space until full-image compaction;
- replacement compaction needs temporary backing capacity and external fencing;
- online segment cleaning, wraparound, shrink and discard remain separate decisions.

## Required proofs before implementation is accepted

A future goal which implements this ADR must prove at least:

- format and open at the 128 MiB minimum;
- sparse reads, writes and checkpoints across logical growth;
- recovery after process loss at every `GROW_LOGICAL` persistence boundary;
- recovery after process loss at every `PROVISION_BACKING` persistence boundary;
- recovery with one old and one relocated root;
- fallback from each corrupted new arena slot to a usable old root;
- rejection when actual backing is smaller than the durable provisioned bound;
- exact skip and range validation for every arena extent;
- round-trip and recovery of WRITE records with 1 and 338 payload blocks;
- rejection of physical counts or append positions above the `u32` format bound;
- idempotent retry after ambiguous `GROW_LOGICAL` and `PROVISION_BACKING` completion;
- checkpoint-arena relocation before mapping capacity is exhausted;
- replacement compaction which never mutates the source before destination verification;
- rejection of unknown record kinds and versions without panic;
- top-level Rust lint and test gates without weakening ADR-03 evidence.

## Open questions

The implementation goal must settle these policies without changing the format invariants above:

- the initial checkpoint-entry capacity and proactive arena-growth watermark;
- the physical-capacity watermark and backing-growth increment;
- whether the first implementation updates a live ublk size or requires detach and reattach;
- when full sparse snapshots justify a paged or copy-on-write checkpoint index;
- when replacement compaction is no longer sufficient and requires segmented online cleaning.
