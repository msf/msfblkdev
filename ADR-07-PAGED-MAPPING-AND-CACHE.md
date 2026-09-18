# ADR-07 Paged mapping and bounded cache

Status: implementation in progress; isolated cache complete
On-disk format: 3
Updates: ADR-01 and ADR-03 for newly formatted version-3 images only
Related: [research proposal](papers/README.md#ideas-to-improve-the-design), [delivery plan](PLAN-PAGED-MAPPING.md)

## Context

Our block device has very high memory usage per storage space it tracks.
The dense mapping consumes 12 bytes per addressable 4 KiB block and rewrites the complete map at each checkpoint. A 4 GiB volume needs 12 MiB, 400 GiB needs 1,200 MiB, and 4 TiB needs 12 GiB. Sparse physical use does not reduce this cost.

Another problem we have is very large checkpoints. The full `dense mapping` is flushed to disk at each checkpoint.

We will store the mapping in independently replaceable 4 KiB log records and cache a bounded subset in RAM. A resident directory locates each mapping page. This reduces checkpoint writes to changed pages plus the complete directory.

ADR-04 already assigns format version 2 to its separate growth proposal. This ADR assigns version 3; it does not implement or inherit version 2. that ADR isn't implemented yet, we've decided instead to fix this high memory usage first.

## Decision

Keep fixed logical and backing sizes, the serialized engine, payload integrity checks, and an append-only finite log. Keep two checkpoint slots and recovery from the older valid slot. Do not add growth, discard, segmentation, cleaning, migration, or concurrency yet.

### What is a mapping page?

```text
page_id = LBA / 320
entry   = LBA % 320
page_count = ceil(volume_blocks / 320)

MappingEntries in RAM
  physical_addr[320]       u32
  payload_checksum[320]    u64

PageIndex in RAM and each checkpoint body
  mapping_page_block[page_count] u32
```

The two entry arrays occupy exactly 3,840 bytes. Separate arrays avoid the per-entry padding of ordinary `{u32, u64}` structs. Use naturally aligned native arrays in RAM and explicit little-endian encoding on disk. Do not use packed references or cast disk buffers into native structs. Direct-I/O alignment applies to the separate I/O buffers, not every cache slot.

Physical address zero means unmapped and requires checksum zero. A zero directory entry means the entire page is unmapped. Entries beyond the logical end of the volume must be zero. The disk record checksum is not retained as authoritative state in a mutable cached page.

Checkpoints will simply contain the location of each mapping entry page, becoming much shorter.

The complete checkpoint consists of a descriptor and its index body. The pointer array belongs to the checkpoint; it does not fit inside the fixed 4 KiB descriptor. These are decoded Rust types, not structures to cast onto disk bytes:

```rust
enum CheckpointSlot { Green = 0, Blue = 1 }

struct CheckpointDescriptor {
    slot: CheckpointSlot,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    checkpoint_lsn: u64,
    last_record_boundary: u32,
    index_checksum: u64,
    generation: u64,
}

struct Checkpoint {
    descriptor: CheckpointDescriptor,
    mapping_page_block: Box<[u32]>, // Exactly page_count pointers.
}

#[repr(u8)]
enum RecordKind { Data = 1, MappingPage = 2 }

struct RecordHeader {
    kind: RecordKind,
    volume_id: u64,
    lsn: u64,
    previous_boundary: u32,
    boundary: u32,
}

struct MappingPageRecord {
    header: RecordHeader, // kind must be MappingPage.
    page_id: u32,
    entries: MappingEntries,
}
```

`mapping_page_block[id]` is a physical 4 KiB block number, not a byte offset. Multiply it by 4096 using `u64` arithmetic to issue I/O. The cache stores only `MappingEntries` plus page identity and runtime state. The codec supplies record framing and metadata checksums when it persists a snapshot. Decode numeric kinds through explicit matching; reject unknown values without casting them to an enum.

### Version-3 records and checkpoint schema

Retain ADR-01's geometry bounds, physical-address units, checksum algorithms, payload checksum inputs, and explicit encoding. All metadata padding is zero and validated. Metadata checksums cover their entire 4 KiB block with the checksum field zeroed.

Use the existing footer framing for both record kinds. The shared fields at offsets 0–31 remain ADR-01's `VBLF` magic, version, kind, zero padding, volume ID, LSN, previous record boundary, and this record boundary. Version is 3. Every record, including a mapping snapshot, consumes one strictly increasing LSN. The boundary is the physical block containing the footer or mapping-page record.

Every data footer and mapping-page record starts with this exact header:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 4 | Magic: `VBLF` |
| 4 | 1 | Format version: `3` |
| 5 | 1 | Record kind: data `1`, mapping page `2` |
| 6 | 2 | Zero padding |
| 8 | 8 | Volume ID, little-endian `u64` |
| 16 | 8 | Record LSN, little-endian `u64` |
| 24 | 4 | Previous record boundary, little-endian `u32` |
| 28 | 4 | This record boundary, little-endian `u32` |

Data records use kind 1, with the same payload/footer layout and 1–338 payload bound as ADR-01. Payload count is `this_boundary - previous_boundary - 1`. Preserve the unique-LBA rule within a data record. This change does not require implementing batching.

The data footer stores 338 little-endian `u32` LBA IDs at bytes 32–1383, 338 little-endian `u64` payload checksums at bytes 1384–4087, and its metadata checksum at bytes 4088–4095. Entries after the derived payload count must be zero. Payload `i` is at physical block `previous_boundary + 1 + i`. Its checksum remains XXH3-64 seeded by the volume ID over the little-endian LBA and physical block followed by the 4096 payload bytes.

Mapping-page records use kind 2 and have no preceding payload. Require `this_boundary = previous_boundary + 1`:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 32 | Shared framing described above |
| 32 | 4 | Page ID (`u32`) |
| 36 | 4 | Zero padding |
| 40 | 1280 | `physical_addr[320]`, little-endian `u32` |
| 1320 | 2560 | `payload_checksum[320]`, little-endian `u64` |
| 3880 | 208 | Zero padding |
| 4088 | 8 | Record checksum |

Both arrays begin at 8-byte-aligned offsets. A directory lookup validates the record checksum, kind, version, volume, expected physical boundary, page ID, entry bounds, and that the snapshot is not newer than the selecting checkpoint. Snapshot records describe mappings; they are not independent logical writes during replay.

The reserved prefix contains descriptors at blocks 0 and 1, followed by the green and blue index bodies. Each body contains `page_count` little-endian `u32` pointers, rounded once to 4 KiB with zero padding. Derive body positions and sizes from validated geometry. The append log starts immediately after both bodies.

The complete 4 KiB checkpoint descriptor has this layout:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 4 | Magic: `VBLC` |
| 4 | 1 | Format version: `3` |
| 5 | 1 | Checkpoint slot: green `0`, blue `1` |
| 6 | 2 | Zero padding |
| 8 | 8 | Volume ID, little-endian `u64` |
| 16 | 4 | Logical block size: `4096`, little-endian `u32` |
| 20 | 4 | Volume block count, little-endian `u32` |
| 24 | 8 | Backing block count, little-endian `u64` |
| 32 | 8 | Checkpoint LSN, including this publication's mapping records |
| 40 | 4 | Last record boundary, little-endian `u32` |
| 44 | 8 | XXH3-64 checksum of the complete padded index body |
| 52 | 8 | Checkpoint generation, little-endian `u64` |
| 60 | 4028 | Zero padding |
| 4088 | 8 | Descriptor checksum, little-endian `u64` |

The index body and its locations are exact functions of the volume geometry:

```text
index_blocks = ceil(page_count * 4 / 4096)
green_body_start = 2
blue_body_start = 2 + index_blocks
log_start = 2 + 2 * index_blocks

body byte offset 4 * page_id: mapping_page_block[page_id], little-endian u32
body bytes page_count * 4 .. index_blocks * 4096: zero padding
```

Both index bodies always occupy their reserved space. The descriptor's slot identifies its body; no body pointer or variable body length is encoded. Every nonzero index pointer must be within the append log and at or before the checkpoint boundary. Every mapped payload address in a snapshot must precede that snapshot's record boundary and remain within the append log. Zero physical addresses require zero payload checksums.

The minimum backing size is `log_start + 3` physical blocks: one payload, its data footer, and the mapping snapshot required to checkpoint that write. The existing maximums remain 2^31 logical blocks and 2^32 backing blocks. Physical addresses remain `u32`; byte offsets and the one-past-end backing count use `u64`.

Generation, not logical write activity, orders checkpoints. Reject generation overflow and conflicting equal-generation checkpoints. Format writes both zero-filled index bodies and valid generation-zero descriptors, then persists them. The initial LSN is zero and boundary is `log_start - 1`. Unlike version 1, there is no special checksum-zero/absent-body encoding.

Validate directory pointer bounds before reading pages. Validate all referenced mapping pages by streaming them before accepting a checkpoint candidate. This deliberately retains full candidate validation and fallback without loading all pages into RAM. Startup therefore reads populated mapping metadata; it is not constant-time.

### What does the RAM budget include?

Default requested cache budget is `floor(logical_bytes / 10_000)`: 0.01%, not 1%. This is an unmeasured starting policy intended to exercise eviction. Permit an explicit byte-budget override for experiments and tiny-cache tests. Select the largest whole-slot capacity that fits, cap it at `page_count`, and allow a minimum one-slot allocation even when it exceeds the requested budget. Report requested and effective bytes.

Cache allocations include entry arrays, slot metadata, replacement-policy state, page-ID-to-slot lookup capacity, and allocation padding visible to the implementation. There must be no unbounded per-access allocations or hidden fallback map. Report allocator/RSS overhead separately rather than claiming the byte budget bounds process RSS.

The resident disk-location index is outside this cache budget:

| Logical size | Index bytes | Default requested cache bytes |
|---:|---:|---:|
| 4 GiB | 13,108 | 429,496 |
| 400 GiB | 1,310,720 | 42,949,672 |
| 4 TiB | 13,421,776 | 439,804,651 |

Use a bounded page-ID-to-slot hash table initially so cache lookup memory scales with cache capacity rather than logical size. Count its actual allocated capacity, including spare buckets and control bytes; do not estimate it as just key/value bytes. If the chosen library cannot expose an allocation bound, resolve that in the isolated-cache delivery before integration.

Bound peak mapping memory as resident index plus one candidate index, cache allocations, and fixed-size streaming/I/O buffers. Validate checkpoint candidates sequentially; do not retain an index per candidate or allocate per-LBA replay state. Report steady and peak memory separately.

### How is the cache isolated?

Implement a mapping-page-specific cache, not a generic caching framework. Keep entry storage, lookup, dirty state, and statistics separate from the CLOCK policy. The policy sees slot IDs and access/insert/remove events. Replacing CLOCK with LRU must not change the disk encoding, engine lookup path, or checkpoint protocol.

CLOCK keeps one referenced bit per slot and a cursor. Access sets the bit. Victim search clears referenced bits and selects an unreferenced clean slot. Search is bounded to two full passes. Dirty pages are never eviction victims during normal operation. When no victim exists, return a typed checkpoint-required outcome; the cache performs no I/O. The engine checkpoints and retries. Invalid capacity and inconsistent slot transitions must have explicit errors or internal invariant checks.

For a write, obtain a cache slot before appending its data record. After exact data-record completion, update the cached mapping and mark it dirty. Do not accept a write and only then discover that publishing its mapping needs an unavailable slot. Process multi-page requests in bounded units without unbounded pinning.

Dirty means that an update has changed the cached mapping since the last successful checkpoint publication. The payload write buffer contains application data, not mapping pages. Buffering a payload alone does not update its physical mapping. Releasing its data record updates the mapping after exact completion and marks the page dirty. A successful data flush leaves the page dirty; only successful checkpoint publication marks it clean. Loading a page from the active checkpoint, or constructing an implicitly unmapped page from a zero index pointer, produces a clean page. Replay updates produce dirty pages as well.

### What must metrics report?

Maintain counters for foreground hits and misses, recovery hits and misses, clean evictions, victim slots examined, and checkpoint-required outcomes. The engine separately counts completed cache-pressure checkpoints, mapping-page reads, mapping-page writes, index bytes written, and checkpoint failures. Report resident slots, dirty slots, slot capacity, cache allocated bytes, and index bytes.

Count one hit or miss per requested page acquisition, before internal retry. A zero-index page absent from the cache is a miss but needs no metadata read. Internal checkpoint work does not inflate foreground hit rate. Hit rate is `hits / (hits + misses)` and is unavailable before any lookup. Keep counters available through a snapshot API and expose them in existing daemon/lab reporting; no metrics-server dependency is required.

Dedicated isolated unit tests must check exact counters as well as cache behavior. Metrics are part of the cache contract, not an optional later delivery.

### When do pages become durable?

FUA and flush requests drain the required buffered data writes and persist their log records before acknowledgement. They need not persist mapping pages or advance the checkpoint LSN. Replay reconstructs those mappings after a crash.

Checkpoint publication serializes with foreground operations:

```text
append dirty mapping-page records and update a candidate index
→ fsync data and mapping records
→ write the inactive slot's complete candidate index
→ fsync the body
→ write the descriptor with a new generation, LSN and boundary
→ fsync the descriptor
→ select the candidate index and mark cached pages clean
```

Unchanged pointers are reused. The old checkpoint and its pages remain untouched. Do not mark pages clean or expose the candidate index as committed before descriptor persistence succeeds. An uncertain backing I/O failure enters the existing failed-volume state; it must not authorize eviction or continued writes.

Reserve enough append space for all resulting dirty-page snapshots before admitting each foreground record. Include the new dirty page and framing in the calculation. Reserved checkpoint bodies need no append space. Return a capacity error before accepting a write that would consume the space needed to checkpoint its mapping. There is still no reclaim or wraparound.

### How does bounded recovery work?

First select and validate a checkpoint and determine the valid replay tail using volume identity, record chaining, strictly consecutive LSNs, bounded record lengths, and checksums. Mapping records participate in traversal but never apply logical updates. Data records update mappings. Unknown versions or kinds are not silently skipped.

Do not assume the cache holds the tail's dirty working set. Older-checkpoint fallback can replay more than one normal checkpoint interval. Never publish a checkpoint at an intermediate replay cursor while unread durable data remains after that cursor.

The initial bounded fallback is multi-pass recovery. Once the complete valid tail is identified and protected, process mapping-page-ID windows that fit the cache. Load their base pages and replay the fixed tail for those IDs. Append snapshots of changed pages into a candidate index, beyond the recovered tail; do not overwrite any replay source. Reuse the window only after its snapshot writes complete. Persist the complete result and publish a single final checkpoint covering the entire recovered tail and its new snapshots. No intermediate candidate becomes a recovery root. A crash before final publication must still recover through the original checkpoint and tail; newly appended snapshots are not logical writes.

This path trades repeated log reads for a strict RAM bound. Skip it when ordinary replay fits the cache. The implementation must define tail sanitation, partial snapshot handling, append reservation, and retry-after-crash ordering before wiring this path into `open`. Insufficient space fails open explicitly without destroying recoverable state. Do not silently allocate the complete map to avoid this case.

### Compatibility and acceptance

The new formatter emits version 3. The new engine rejects versions 1, 2, and unknown versions without modifying those images. Do not add in-place conversion or silently reformat on open. Historical version-1 support, if desired later, is a separate explicit path.

Required evidence includes byte-layout fixtures, malformed-record rejection, isolated cache tests and metrics, pressure-triggered checkpoints, unchanged-page reuse, flush without checkpoint, older-checkpoint fallback, bounded replay beyond cache capacity, and interruption at each publication boundary. Preserve existing payload validation and filesystem correctness gates.

## Consequences

A cache miss can add a mapping-page read before the payload read. Small caches can cause frequent checkpoints under scattered writes. CLOCK is approximate LRU; neither its hit rate nor the default budget is established by measurement.

Checkpoint bodies shrink to four bytes per 320 logical blocks, rounded to 4 KiB. Mapping-page writes depend on dirty pages rather than the complete map. The resident index still grows with logical capacity, and peak checkpoint memory includes a candidate copy.

Old mapping and data records still fill the finite log. Multi-pass recovery and full checkpoint-page validation can be expensive. These are explicit initial costs, not sustained-overwrite or fast-startup claims.

The delivery plan must resolve and test the identified recovery and allocation details before enabling version 3 as the default format.

## Deliveries and validation

Each delivery uses isolated commits. Checkboxes record executed evidence, not intended behavior. The [delivery plan](PLAN-PAGED-MAPPING.md) provides the detailed test matrix.

### Delivery 1: isolated mapping cache

- [x] Implement fixed slots, preallocated lookup, separate CLOCK policy, dirty-page iteration, and typed pressure/retry outcomes.
- [x] Verify exact counters, layout, budget rounding, one-slot minimum, page-count cap, dirty protection, and a deterministic reference model.
- [x] Measure zero allocations during hits, updates, iteration, retry, and 110,000 replacements across capacity boundaries.
- [x] Pass the repository gate. Evidence: commit `7a3617f`; 16 isolated cache tests and all 234 `make lint test` commands passed on 2026-09-18. No engine integration or persistence behavior is established by these tests.

### Delivery 2: version-3 codecs and checkpoint storage

- [x] Implement typed shared framing. Evidence (2026-09-18): three `format_v3` tests verify exact bytes, independent checksums, malformed headers, all 1–338 payload lengths, physical bounds, and consecutive mixed-kind chains. `make lint test` passed all 237 commands.
- [x] Implement mapping snapshots and fixed geometry. Evidence (2026-09-18): four additional isolated tests verify both arrays, padding, checksum coverage, partial pages, pointer/identity rejection, and geometry limits. All seven `format_v3` tests and all 241 repository gate commands passed.
- [x] Implement data footers. Evidence (2026-09-18): three additional tests verify all 338 payload counts, exact arrays and checksums, malformed entries, and complete mixed-kind record chains. All ten `format_v3` tests and all 244 repository gate commands passed.
- [x] Implement checkpoint descriptors and generation consistency checks. Evidence (2026-09-18): three additional tests verify every descriptor offset, checksum, malformed field rejection, initial state, equal-generation conflicts, and generation overflow. All 13 codec tests and all 247 repository gate commands passed.
- [ ] Implement streamed index bodies and a test-only formatter.
- [ ] Verify mixed record chains, partial pages, checksums, zero padding, overflow, and non-mutating unsupported-version rejection.
- [ ] Document and test append reservation and bounded recovery publication ordering before engine integration.

### Delivery 3: engine and frontend integration

- [ ] Replace dense maps with the resident page index and cache; acquire mapping capacity before data-record append.
- [ ] Publish dirty snapshots and the candidate index in the specified fsync order; preserve flush/FUA semantics and failed-volume behavior.
- [ ] Implement bounded recovery, older-checkpoint fallback, smaller-cache reopen, and explicit space-exhaustion handling.
- [ ] Activate version 3 only when open/read/write/recovery work; expose cache and mapping-I/O statistics in engine and daemon/lab reports.

### Delivery 4: acceptance and completion

- [ ] Prove interruption at mapping, index, descriptor, and recovery publication boundaries, including repeated recovery crashes.
- [ ] Prove pressure checkpoints, unchanged-page reuse, flush without checkpoint, malformed metadata rejection, and fallback after snapshot corruption.
- [ ] Record cache/index/peak mapping memory and separate RSS measurements, with exact access-sequence hit rates.
- [ ] Pass `make lint test`, `make test-acceptance`, and operator-approved live ublk, ext4, XFS, and SQLite regression gates. Update documentation and create the completion tag only after all required evidence passes.
