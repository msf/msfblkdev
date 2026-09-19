# ADR-07 Paged mapping and bounded cache

Status: complete; all engine, memory, and live acceptance gates passed (2026-09-19)
Latest release: v1.2.2; v1.2.3 refinement and validation in progress. v1.2.1 contains the packed-data correction; v1.2 preserves the original release.
On-disk format: 3
Updates: ADR-01 and ADR-03 for newly formatted version-3 images only
Related: [research proposal](papers/README.md#ideas-to-improve-the-design), [delivery plan](PLAN-PAGED-MAPPING.md)

## Context

Our block device has very high memory usage per storage space it tracks.
The dense mapping consumes 12 bytes per addressable 4 KiB block and rewrites the complete map at each checkpoint. A 4 GiB volume needs 12 MiB, 400 GiB needs 1,200 MiB, and 4 TiB needs 12 GiB. Sparse physical use does not reduce this cost.

Another problem we have is very large checkpoints. The full `dense mapping` is flushed to disk at each checkpoint.

We will store the mapping in independently replaceable 4 KiB log records and cache a bounded subset in RAM. A resident directory locates each mapping page. This reduces checkpoint writes to changed pages plus the complete directory.

ADR-04 already assigns format version 2 to its separate growth proposal. This ADR assigns version 3; it does not implement or inherit version 2. ADR-04 is not implemented; we prioritized reducing mapping memory.

Format 3 remains a prototype under active refinement. The user explicitly permits
incompatible layout changes within version 3. The 336-entry layout replaces the
320-entry layout used through v1.2.2. Reformat disposable images; no migration or
compatibility with earlier format-3 images is promised. Release tags retain the old
implementation and measurements. This refinement does not implement ADR-04.

## Decision

Keep fixed logical and backing sizes, the serialized engine, payload integrity checks, and an append-only finite log. Keep two checkpoint slots and recovery from the older valid slot. Do not add growth, discard, segmentation, cleaning, migration, or concurrency yet.

### What is a mapping page?

```text
page_id = LBA / 336
entry   = LBA % 336
page_count = ceil(volume_blocks / 336)

MappingEntries in RAM
  physical_addr[336]       u32
  payload_checksum[336]    u64

PageIndex in RAM and each checkpoint body
  mapping_page_block[page_count] u32
```

The two entry arrays occupy exactly 4,032 bytes. Separate arrays avoid the per-entry padding of ordinary `{u32, u64}` structs. Use naturally aligned native arrays in RAM and explicit little-endian encoding on disk. Do not use packed references or cast disk buffers into native structs. Direct-I/O alignment applies to the separate I/O buffers, not every cache slot.

Physical address zero means unmapped and requires checksum zero. A zero directory entry means the entire page is unmapped. Entries beyond the logical end of the volume must be zero. The disk record checksum is not retained as authoritative state in a mutable cached page.

Checkpoints will simply contain the location of each mapping entry page, becoming much shorter.

The complete checkpoint consists of a descriptor and its index body. The pointer array belongs to the checkpoint; it does not fit inside the fixed 4 KiB descriptor. These are decoded Rust types, not structures to cast onto disk bytes:

```rust
#[repr(u8)]
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

struct MappingEntries {
    physical_addr: [u32; 336],
    payload_checksum: [u64; 336],
}

struct MappingPageRecord {
    header: RecordHeader, // kind must be MappingPage.
    page_id: u32,
    entries: MappingEntries,
}
```

`mapping_page_block[id]` is a physical 4 KiB block number, not a byte offset. Multiply it by 4096 using `u64` arithmetic to issue I/O. The cache stores only `MappingEntries` plus page identity and runtime state. The codec supplies record framing and metadata checksums when it persists a snapshot. Decode numeric kinds through explicit matching; reject unknown values without casting them to an enum.

### Version-3 records and checkpoint schema

Retain ADR-01's geometry bounds, physical-address units, checksum algorithms, payload checksum inputs, and explicit encoding. All metadata padding is zero and validated. Record and descriptor checksums cover their entire 4 KiB block with the final checksum field zeroed. Mapping snapshots also carry the independent header checksum defined below.

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

Data records use kind 1, with the same payload/footer layout and 1–338 payload bound as ADR-01. Payload count is `this_boundary - previous_boundary - 1`. Preserve the unique-LBA rule within a data record. Records can contain LBAs from multiple mapping pages. The writer packs records after sorting the complete write buffer by LBA; the decoder also accepts existing unsorted records.

The data footer stores 338 little-endian `u32` LBA IDs at bytes 32–1383, 338 little-endian `u64` payload checksums at bytes 1384–4087, and its metadata checksum at bytes 4088–4095. Entries after the derived payload count must be zero. Payload `i` is at physical block `previous_boundary + 1 + i`. Its checksum remains XXH3-64 seeded by the volume ID over the little-endian LBA and physical block followed by the 4096 payload bytes.

Mapping-page records use kind 2 and have no preceding payload. Require `this_boundary = previous_boundary + 1`:

| Offset | Bytes | Field |
|---:|---:|---|
| 0 | 32 | Shared framing described above |
| 32 | 4 | Page ID (`u32`) |
| 36 | 4 | Zero padding |
| 40 | 1344 | `physical_addr[336]`, little-endian `u32` |
| 1384 | 2688 | `payload_checksum[336]`, little-endian `u64` |
| 4072 | 8 | Header checksum: XXH3-64 over bytes 0–39, little-endian `u64` |
| 4080 | 8 | Zero padding |
| 4088 | 8 | Record checksum |

Both arrays begin at 8-byte-aligned offsets. A directory lookup validates the record checksum, kind, version, volume, expected physical boundary, page ID, entry bounds, and that the snapshot is not newer than the selecting checkpoint. Snapshot records describe mappings; they are not independent logical writes during replay.

The header checksum includes the shared framing, page ID, and padding through byte 39. The full record checksum also covers this header checksum. Mapping-page decoding requires both checksums. Replay may traverse a snapshot whose entries or full record checksum are damaged only when its independent header checksum, identity, geometry, and consecutive chain position validate. It must not use that snapshot's entries. Corrupted framing is an error, not evidence of a clean end of log.

This second checksum is required for older-checkpoint fallback. With only a full record checksum, one damaged mapping entry invalidates the snapshot and its traversal fields together. Recovery could select the older root but could not safely reach later flushed data records. Using eight reserved bytes separates these checks without moving either entry array or increasing the record size.

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

The base cache budget is `floor(logical_bytes / 10_000)`: 0.01%, not 1%. The automatic budget is the larger of that base and the allocation needed for `min(effective_write_buffer_slots, page_count)` mapping pages. The effective buffer capacity includes the backing-size clamp. This minimum covers the worst case where every buffered payload belongs to a different mapping page. It is a configuration bound, not a prediction of locality.

Permit an explicit byte-budget override. Select the largest whole-slot capacity that fits, cap it at `page_count`, and retain the cache's one-slot allocation minimum. Reject an explicit configuration whose resulting slot capacity is below the write-buffer requirement. Validate it after reading checkpoint geometry but before recovery or tail sanitation can write. Report the required slots and minimum bytes in the error; do not silently enlarge an explicit cache budget.

Cache allocations include entry arrays, slot metadata, replacement-policy state, page-ID-to-slot lookup capacity, and allocation padding visible to the implementation. There must be no unbounded per-access allocations or hidden fallback map. Report allocator/RSS overhead separately rather than claiming the byte budget bounds process RSS.

The resident disk-location index is outside this cache budget:

| Logical size | Index bytes | Base cache bytes, before the write-buffer minimum |
|---:|---:|---:|
| 4 GiB | 12,484 | 429,496 |
| 400 GiB | 1,248,308 | 42,949,672 |
| 4 TiB | 12,782,644 | 439,804,651 |

Use a bounded page-ID-to-slot hash table initially so cache lookup memory scales with cache capacity rather than logical size. Count its actual allocated capacity, including spare buckets and control bytes; do not estimate it as just key/value bytes. If the chosen library cannot expose an allocation bound, resolve that in the isolated-cache delivery before integration.

Bound peak mapping memory as resident index plus one candidate index, cache allocations, and fixed-size streaming/I/O buffers. Validate checkpoint candidates sequentially; do not retain an index per candidate or allocate per-LBA replay state. Report steady and peak memory separately.

### How is the cache isolated?

Implement a mapping-page-specific cache, not a generic caching framework. Keep entry storage, lookup, dirty state, and statistics separate from the CLOCK policy. The policy sees slot IDs and access/insert/remove events. Replacing CLOCK with LRU must not change the disk encoding, engine lookup path, or checkpoint protocol.

CLOCK keeps one referenced bit per slot and a cursor. Access sets the bit. Victim search clears referenced bits and selects an unreferenced clean slot. Search is bounded to two full passes. Dirty pages are never eviction victims during normal operation. When no victim exists, return a typed checkpoint-required outcome; the cache performs no I/O. The engine checkpoints and retries. Invalid capacity and inconsistent slot transitions must have explicit errors or internal invariant checks.

Before appending a buffered group, ensure that cache capacity covers its mapping pages and all existing dirty pages. Publish a checkpoint first if needed. After exact data-record completion, acquire each required mapping page, update its entries, and mark it dirty. The configuration minimum and preflight guarantee slot availability throughout the group without pinning clean pages in advance. A mapping read or validation failure still latches the volume as failed.

Dirty means that an update has changed the cached mapping since the last successful checkpoint publication. The payload write buffer contains application data, not mapping pages. Buffering a payload alone does not update its physical mapping. Releasing its data record updates the mapping after exact completion and marks the page dirty. A successful data flush leaves the page dirty; only successful checkpoint publication marks it clean. Loading a page from the active checkpoint, or constructing an implicitly unmapped page from a zero index pointer, produces a clean page. Replay updates produce dirty pages as well.

### What must metrics report?

Maintain counters for foreground hits and misses, recovery hits and misses, clean evictions, victim slots examined, and checkpoint-required outcomes. The engine separately counts completed cache-pressure checkpoints, mapping-page reads, mapping-page writes, index bytes written, and checkpoint failures. Report resident slots, dirty slots, slot capacity, cache allocated bytes, and index bytes.

Count one hit or miss per requested page acquisition, before internal retry. A zero-index page absent from the cache is a miss but needs no metadata read. Internal checkpoint work does not inflate foreground hit rate. Hit rate is `hits / (hits + misses)` and is unavailable before any lookup. Keep counters available through a snapshot API and expose them in existing daemon/lab reporting; no metrics-server dependency is required.

Mapping-page I/O counters count exact completed operations through checkpoint pointers or snapshot appends, including work before a later validation or publication failure. Index bytes written include each completed padded index block and exclude descriptors. Count all tail-scan block reads separately: scans read payloads and metadata while locating record boundaries. These I/O counters do not imply durability or successful publication.

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

Checkpoint pressure publishes all current dirty pages. Starting with v1.2.2, the writer batches consecutive mapping snapshots and index-body blocks into writes of at most 32 blocks (128 KiB), using bounded aligned scratch storage. Each snapshot retains its own framing, consecutive LSN, and checksums. Require exact completion of each batched write before counting its pages as written. A short or failed write fails the publication and latches the volume; never retry an uncertain append or clear dirty flags. Preserve the existing three publication barriers. Batching changes I/O submission size, not the disk format or the checkpoint's atomic publication boundary.

A batch is not an atomic disk write. A crash can leave complete records, missing blocks, or torn metadata. Recovery may traverse a valid consecutive prefix of snapshot records without adopting their entries. Invalid recognizable snapshot framing remains a fail-closed error under the existing format contract. Do not publish an index pointing into a batch until all required writes and barriers succeed.

Reserve enough append space for all resulting dirty-page snapshots before admitting each foreground record. Include the new dirty page and framing in the calculation. Reserved checkpoint bodies need no append space. Return a capacity error before accepting a write that would consume the space needed to checkpoint its mapping. There is still no reclaim or wraparound.

On release, sort the complete payload write buffer by LBA, then split it into packed data records of at most 338 payloads. Mapping-page boundaries do not split records. Sort the LBA sequence while retaining each payload's arena location; compute checksums using the resulting physical offsets. Repeated writes to an LBA still coalesce before release.

Before the first record, count the union of currently dirty mapping pages and pages touched by this buffer. If that union exceeds cache capacity, publish one checkpoint first. Also checkpoint first if the complete packed group would exceed the configured replay-byte bound. The checkpoint interval must accommodate the largest effective packed buffer; reject an incompatible interval before recovery writes. After this preflight, neither cache pressure nor the replay threshold may publish a checkpoint within the group.

Submit each packed record and wait for its exact write completion before applying its mapping entries. A completed record may update several mapping pages, and a mapping page may span consecutive records. Dirty pages remain protected throughout the group. Advance the in-memory log position after all mappings for that record have been applied. On any failure, latch the volume as failed; do not publish a partial record's mapping state. Recovery replays complete records from the previous checkpoint after a crash.

A flush drains the required complete group and issues **one data-log fsync for the group**, not one per packed record. Filling the buffer releases records without an unconditional fsync. A checkpoint before a group has its own existing log, index-body, and descriptor barriers. Those checkpoint barriers are separate from the group's final data-log barrier. Close may persist the group through checkpoint publication.

The group is not an atomic transaction. Before a flush succeeds, recovery may retain a prefix of complete records. A successful flush guarantees recovery of all its records.

Before accepting a new buffered LBA, let `N` be the resulting payload count, `R = ceil(N / 338)` its packed record count, `P` its distinct mapping-page count, and `D` the current dirty-cache page count. Reserve `N + R + D + P` physical blocks: payloads, data footers, existing dirty snapshots, and snapshots for pending updates. Count existing and pending page sets separately even when they overlap: the preflight checkpoint can persist an old dirty page before this group updates it.

No checkpoint occurs inside the group. Each pending mapping page therefore needs at most one later snapshot, even when its updates cross a packed-record boundary. A buffered overwrite of an existing LBA adds no reservation. Capacity rejection occurs before modifying the buffer or issuing backing I/O.

This write-batching amendment supersedes v1.2's one-mapping-page-per-record rule. The [v1.1/v1.2 measurements](ADR-07-PERF-RESULTS.md#historical-measurements-before-the-batching-fix) found 4,080 backing writes instead of 128 for the same scattered batch. The minimum compatible cache replaces that rule without changing the version-3 disk layout. Historical delivery entries below describe the original v1.2 implementation.

The [packed-write measurements](ADR-07-PERF-RESULTS.md) repeat the release baselines and report the amended implementation's throughput, latency, memory, and checkpoint costs.

### How does bounded recovery work?

First select and validate a checkpoint and determine the valid replay tail using volume identity, record chaining, strictly consecutive LSNs, bounded record lengths, and checksums. Mapping records participate in traversal but never apply logical updates. Data records update mappings. Unknown versions or kinds are not silently skipped.

Do not assume the cache holds the tail's dirty working set. Older-checkpoint fallback can replay more than one normal checkpoint interval. Never publish a checkpoint at an intermediate replay cursor while unread durable data remains after that cursor.

The initial bounded fallback is multi-pass recovery. Once the complete valid tail is identified and protected, process mapping-page-ID windows that fit the cache. Load their base pages and replay the fixed tail for those IDs. Append snapshots of changed pages into a candidate index, beyond the recovered tail; do not overwrite any replay source. Reuse the window only after its snapshot writes complete. Persist the complete result and publish a single final checkpoint covering the entire recovered tail and its new snapshots. No intermediate candidate becomes a recovery root. A crash before final publication must still recover through the original checkpoint and tail; newly appended snapshots are not logical writes.

This path trades repeated log reads for a strict RAM bound. Skip it when ordinary replay fits the cache. The implementation must define tail sanitation, partial snapshot handling, append reservation, and retry-after-crash ordering before wiring this path into `open`. Insufficient space fails open explicitly without destroying recoverable state. Do not silently allocate the complete map to avoid this case.

The initial scan uses the candidate index itself to mark touched pages with the temporary value `1`. Valid disk pointers start at block 4 or later, so this marker is unambiguous and is never persisted. Reserve one appended snapshot per distinct touched page before any recovery write. If all touched pages fit, replay once into the cache and retain them dirty; the active checkpoint remains unchanged.

Otherwise, each window contains at most the cache capacity in touched page IDs. Every pass stops at the original scanned tail, even after recovery appends snapshots. Replace planning markers with snapshot pointers as each window completes. Drop that window's cache only after exact snapshot-write completion, before allocating the next window's cache. Keep the active index and original replay source intact until the final descriptor fsync. Peak ownership is one cache, the active index, one candidate index, and fixed I/O/record buffers.

An incomplete data footer ends the valid prefix as in ADR-01. A snapshot with valid independent framing can be traversed even if its body is partial or corrupt. Detected invalid snapshot framing or unsupported records fail open before recovery writes; they do not authorize tail clearing. After successful recovery, sanitation starts after the returned tail (including newly published snapshots), clears at most 339 blocks, and fsyncs before foreground operations begin.

### Compatibility and acceptance

The new formatter emits version 3. The new engine rejects versions 1, 2, and unknown versions without modifying those images. Do not add in-place conversion or silently reformat on open. Historical version-1 support, if desired later, is a separate explicit path.

Required evidence includes byte-layout fixtures, malformed-record rejection, isolated cache tests and metrics, pressure-triggered checkpoints, unchanged-page reuse, flush without checkpoint, older-checkpoint fallback, bounded replay beyond cache capacity, and interruption at each publication boundary. Preserve existing payload validation and filesystem correctness gates.

## Consequences

A cache miss can add a mapping-page read before the payload read. Small caches can cause frequent checkpoints under scattered writes. CLOCK is approximate LRU. The deterministic access measurements below establish accounting for those sequences; they do not establish a production hit rate or justify the default budget for a particular workload.

Checkpoint bodies shrink to four bytes per 336 logical blocks, rounded to 4 KiB. Mapping-page writes depend on dirty pages rather than the complete map. The resident index still grows with logical capacity, and peak checkpoint memory includes a candidate copy.

Old mapping and data records still fill the finite log. Multi-pass recovery and full checkpoint-page validation can be expensive. These are explicit initial costs, not sustained-overwrite or fast-startup claims.

The delivery plan must resolve and test the identified recovery and allocation details before enabling version 3 as the default format.

## Deliveries and validation

Each delivery uses isolated commits. Checkboxes record executed evidence, not intended behavior. Entries are chronological; a limitation describes the state at that step. The final entries record completion. The [delivery plan](PLAN-PAGED-MAPPING.md) provides the detailed test matrix.

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
- [x] Implement the complete checkpoint with its owned pointer array and streamed index codec. Evidence (2026-09-18): three additional tests verify multi-block bodies, padded checksums, invalid-pointer rejection without candidate mutation, and zero allocations during streaming. All 16 codec tests and all 250 repository gate commands passed.
- [x] Implement a test-only formatter. Evidence (2026-09-18): three file-backed tests verify both persisted bodies and descriptors, untouched log bytes, non-mutating geometry rejection, and exact write/fsync accounting. All 19 version-3 tests and all 253 repository gate commands passed. The public formatter still emits version 1.
- [x] Implement isolated checkpoint loading and candidate fallback. Evidence (2026-09-18): six file-backed tests cover multi-block indexes, the final partial page, corrupted snapshots/indexes, conflicting roots, backing-read failures, and exact read accounting. Loading validates candidates sequentially and never writes. All 265 repository gate commands passed; all six focused tests also passed after adding a valid-range pointer corruption case.
- [x] Verify mixed record chains, partial pages, checksums, zero padding, overflow, and non-mutating unsupported-version rejection in the isolated codecs and loader. A captured version-1 formatter fixture proves rejection independently of future public formatter changes. Versions 2 and 255 are also rejected without writing. Public `open` activation remains in Delivery 3.
- [x] Document and test foreground append reservation. Evidence (2026-09-18): three isolated tests cover exact charges, capacity/overflow limits, and exhaustive small dirty-set/pressure schedules. All 256 repository gate commands passed. Engine integration must still enforce the tested page-grouping premise.
- [x] Implement isolated buffered-page grouping. Evidence (2026-09-18): three tests cover interleaved pages, coalesced payloads, agreement with reservation at the 4,096-payload buffer limit, and allocation-free grouping. All 259 repository gate commands passed. The production release path remains unchanged until engine integration.
- [x] Separate snapshot traversal integrity from mapping contents. Evidence (2026-09-18): independent header-checksum tests flip every byte of a 4 KiB snapshot, reject damaged headers and mapping state, and retain valid traversal fields after body damage. Additional cases reject invalid framing with a recomputed checksum. All 271 repository gate commands passed. Replay integration remains pending.
- [x] Implement isolated mixed-record tail traversal. Evidence (2026-09-18): four file-backed tests cover footer-size bounds, incomplete data appends, invalid cursors, unsupported records, detected mapping-header damage, and application payloads resembling metadata. Older-root fallback traverses a damaged snapshot body and reaches a later flushed write with its original payload checksum. All 275 repository gate commands passed. Applying the tail through bounded cache windows remains pending.
- [x] Prove bounded recovery publication ordering before engine integration. Evidence (2026-09-18): recovery scans and reserves before writing, stops every pass at the fixed replay boundary, appends snapshots beyond that boundary, and selects one complete root. The recovery crash matrix below verifies interruption between windows and repeated retries without changing logical mappings.

### Delivery 3: engine and frontend integration

- [x] Implement isolated checkpoint publication. Evidence (2026-09-18): three file-backed tests verify dirty-snapshot persistence, unchanged-pointer reuse, preservation of the old root, pre-write tail/capacity/generation rejection, and write-budget failures at snapshot, index, and descriptor stages. Successful publication accounts for three fsyncs. All 268 repository gate commands passed. Crash interruption and engine failed-volume integration remain unverified.
- [x] Implement isolated bounded recovery. Evidence (2026-09-18): three file-backed tests compare one-slot replay with the fitting-cache path over 338 mappings, verify fixed-source preservation and exact recovery lookup counts, load unchanged entries during overwrite, reopen the published root, recover through snapshot-body corruption, and reject insufficient append space without changing any backing byte. All 278 repository gate commands passed. Recovery interruption tests and public engine integration remain pending.
- [x] Integrate staged version-3 open and foreground reads. Evidence (2026-09-18): four tests cover dirty-pressure publication and retry with exact counters, zero-page eviction and partial-page bounds, metadata-failure latching without changing read output, and non-mutating version-1 rejection before sanitation. All 284 repository gate commands passed. The staged engine is still test-only; writes and public activation remain pending.
- [x] Integrate staged grouped writes, flush, and close. Evidence (2026-09-18): five tests cover interleaved pages, coalesced overwrites, one-slot pressure, a 320-payload release with zero measured allocations, flush recovery without checkpoint publication, rejection before buffering when snapshot space is exhausted, and failed-I/O latching before mapping updates. All 289 repository gate commands passed. Public activation and frontend validation remain pending.
- [x] Account for live mappings and consumed log space without a dense map. Evidence (2026-09-18): checkpoint validation counts mapped entries while streaming pages; replay and completed writes count only unmapped-to-mapped transitions. Tests verify buffered/coalesced writes, overwrites, pressure snapshots, smaller-cache recovery, clean reopen, failed writes, and fallback without double counting. All 290 repository gate commands passed.
- [x] Expose staged mapping statistics and a report formatter. Evidence (2026-09-18): exact-counter tests cover cache lookups, pressure retries, unavailable hit rates, recovery lookups, snapshot reads, tail scans, index writes, and partial publication failures. Completed I/O remains counted when publication fails; failed publication does not count as a completed pressure checkpoint. All 292 repository gate commands passed. Public API and daemon/lab wiring remain pending.
- [x] Bound admission work to one scan of the pending payloads for page membership. Evidence (2026-09-18): the engine retains the pending distinct-page count and clears it after release. Reference tests verify every admission across interleaved page sequences up to 4,096 payloads, with zero measured allocations. An optimized, admission-only probe for 4,096 distinct pages took 3,749,600 microseconds before the change and 6,740 after it on the same host. These single-run measurements exclude I/O and are not device throughput results. All 292 repository gate commands passed.
- [x] Replace dense maps with the resident page index and cache; acquire mapping capacity before data-record append. Evidence (2026-09-18): public format/open now use version 3; the version-1 engine and checkpoint runtime are removed. Existing raw-record, overwrite, capacity, checkpoint, accounting, and crash fixtures now exercise paged storage. The 296-command repository gate passed, followed by the new public cache-budget test and lint. `VolumeOpenOptions::cache_budget_bytes` and `Volume::mapping_stats()` expose cache configuration and reporting.
- [x] Publish dirty snapshots and the candidate index in the specified fsync order; preserve flush/FUA semantics and failed-volume behavior. Evidence (2026-09-18): migrated library and ublk adapter tests pass through the public paged engine, including flush/reopen, I/O failure, pressure publication, exact accounting, and unchanged output on payload corruption. Live-device acceptance remains pending.
- [x] Implement bounded recovery, older-checkpoint fallback, smaller-cache reopen, and explicit space-exhaustion handling. Evidence: the isolated recovery matrix and complete staged-engine crash matrix above pass; public-engine regression fixtures now verify fallback, all 1–338 payload counts, bounded tail clearing, and rejection before space exhaustion can consume checkpoint reservations. Exhaustive public-engine acceptance remains pending.
- [x] Activate version 3 only when open/read/write/recovery work; expose cache and mapping-I/O statistics in engine and daemon/lab reports. Evidence (2026-09-18): daemon `--mapping-cache-bytes` reaches the engine; info records report mapping statistics at open and before close through the existing stderr evidence capture. A real adapter test verifies a FUA request across two mapping pages, pressure publication, and recovery without close. All 298 repository gate commands passed. Live report capture remains part of final acceptance.

### Delivery 4: acceptance and completion

- [x] Exercise isolated publication with the existing SIGKILL harness. Evidence (2026-09-18): all eight boundaries passed 20 repetitions (160 crashes), covering snapshot append, log fsync, both index blocks, index fsync, descriptor write, and descriptor fsync. Each run selected the expected root and retained the flushed data record and its payload checksum. All 269 repository gate commands passed. This proves process-crash behavior for publication; tail replay and power-loss behavior remain unverified.
- [x] Exercise isolated bounded recovery with the same SIGKILL harness. Evidence (2026-09-18): nine recovery boundaries passed 20 repetitions (180 crashes); three consecutive interruptions of one image also recovered successfully. Every final root retained all 338 payload mappings and checksums. Completed uncommitted snapshots advanced framing only, without becoming logical writes. The publication and recovery matrix passed all 343 crashes, and all 280 repository gate commands passed. Public engine integration remains pending; these tests do not simulate power loss.
- [x] Exercise the complete staged engine before public activation. Evidence (2026-09-18): ten foreground/pressure boundaries and two buffer/flush boundaries each passed 20 SIGKILL repetitions (240 crashes). Recovery preserved the completed group and did not invent later buffered groups; a smaller cache recovered every flushed group. Additional tests verify two checkpoint-threshold crossings across reopen, invalid-write rejection before publication, payload corruption without output mutation, and mapped zero overwrites. All 296 repository gate commands passed. Public API/frontend activation and final acceptance remain pending; process crashes do not simulate power loss.
- [x] Prove interruption at mapping, index, descriptor, and recovery publication boundaries, including repeated recovery crashes. Evidence (2026-09-18): `make test-acceptance` passed all 297 commands at public-engine commit `6bfb347`, including 20 repetitions per crash scenario and every stale-tail clearing boundary. Evidence: `evidence/engine-crash-1789766272869232313-6bfb347dd61fc381b717adc3b2aec77782403a1e.log` (`result: PASS`). This is regular-file process-crash evidence, not simulated power loss.
- [x] Prove pressure checkpoints, unchanged-page reuse, flush without checkpoint, malformed metadata rejection, and fallback after snapshot corruption. Evidence: the publication, bounded recovery, staged-engine, and migrated public-engine tests above all passed in the exhaustive acceptance run at `6bfb347`.
- [x] Record cache/index/peak mapping memory and separate RSS measurements, with exact access-sequence hit rates. Evidence (2026-09-18): the release-build `mapping_cache_profile` example checks four 4,096-read sequences through the public engine. Default caches for 4 GiB, 400 GiB, and 4 TiB logical volumes allocate 429,464, 42,947,552, and 439,802,624 bytes. [The measurement report](evidence/adr7-mapping-profile-2026-09-18.md) records slots, lookup/policy bytes, index sizes, calculated checkpoint/recovery heap bounds, and separate `smaps_rollup` RSS snapshots. These mapping heap bounds exclude fixed buffers and allocator overhead; RSS snapshots are not process peak measurements. `make lint` and all 298 `make test` commands passed.
- [x] Pass the operator-approved live ublk/fio gate. Evidence (2026-09-19): `make test-ublk-fio` passed all seven scenarios with three repetitions each at `1e741be`, including graceful restart, SIGKILL restart, descriptor interruption, and exact ENOSPC behavior. The daemon's mapping statistics appeared in the captured live reports. Log: `evidence/ublk-fio-1789774139742756390-1e741be6fd474623720e9d0c4fa0406c2bde4a38.log` (`result: PASS`, 26.041 seconds).
- [x] Pass the authorized ext4 and XFS regression gates. Evidence (2026-09-19): `make test-ext4` and `make test-xfs` each passed three fresh-image lifecycles, all 12 filesystem checks, content verification across clean restart, request/flag checks, and fixture cleanup. Logs: `evidence/ext4-1789774403275866806.log` and `evidence/xfs-1789774443341096331.log`, both `result: PASS`. Live mapping statistics were captured at open and before close.
- [x] Pass the authorized SQLite regressions. Evidence (2026-09-19): `make test-sqlite-ext4` passed all 87 exact selections in 116.849 seconds; the manifest, summary, and every case result were checked for completeness and zero failures. Evidence directory: `evidence/sqlite-1789774471226558259`. The separate `make test-sqlite` WAL/FULL probe also passed: `evidence/sqlite-1789774611754898730`. These filesystem/database gates verify clean-restart behavior, not power-loss durability.
- [x] Complete the acceptance audit and documentation. `make lint`, all 298 normal test commands, the exhaustive crash matrix, live ublk/fio, ext4, XFS, and both SQLite gates passed. The engine source is unchanged since the exhaustive crash run; later frontend configuration/reporting changes passed the normal and live gates. README, this ADR, and the delivery plan record format compatibility, exact schemas, measured allocations, hit rates, and evidence. Create the completion tag only after committing this final record.

### v1.2.1 packed-write amendment (2026-09-19)

Implemented whole-buffer sorting, 338-payload packing across mapping pages, and cache/checkpoint configuration validation. Tests cover 338, 339, 1,024, and 1,500 scattered payloads, exact record offsets, one final data-log fsync, dirty-page overlap, pressure before release, partial in-memory mapping updates, and checkpoint interruptions. No disk-format or dependency change was needed.

`make lint test` passed all 301 commands. The exhaustive process-crash gate, 21 live ublk/fio cases, ext4 and XFS lifecycles, and all 87 SQLite/ext4 cases passed. [The delivery plan](PLAN-PAGED-MAPPING.md#packed-write-amendment-validation-2026-09-19) lists the evidence. [The performance report](ADR-07-PERF-RESULTS.md) retains 105 engine and 60 fio measurements against both earlier release tags, including the remaining flush-tail and checkpoint costs. The corrected implementation is released as v1.2.1. Raw benchmark rows retain their pre-release label `working`.

### Checkpoint batching validation (2026-09-19)

The checkpoint writer groups at most 32 contiguous snapshot or index blocks into each positional io_uring write. It uses fixed 128 KiB aligned scratch arrays; there is no new runtime dependency or disk-format change. A pressure checkpoint still publishes every dirty page, and the cache becomes clean only after the descriptor fsync.

`make lint test test-acceptance` passed all 305 commands in both bounded and exhaustive modes. New tests verify 1/2/31/32/33/65 dirty pages, 1/32/33 index blocks, exact completed write counts and bytes, consecutive snapshot LSNs, failed-publication latching after a successful earlier batch, and replay with holes before later snapshot records. Eight batched-publication boundaries passed 20 SIGKILL repetitions each in exhaustive mode. A further short-write test verifies that a sub-page write missing the independent header checksum leaves the old root valid and reopening fails closed without changing the backing. Focused tests and lint passed again after adding that case.

The existing authorized live ublk/fio, ext4, XFS, and all 87 SQLite/ext4 cases passed. [The delivery plan](PLAN-PAGED-MAPPING.md#checkpoint-batching-validation-2026-09-19) records the evidence. The [performance rerun](ADR-07-PERF-RESULTS.md#v122-checkpoint-batching-2026-09-19) passed 105 engine, 60 fio, and 25 I/O-accounting cases. Automatic-cache scattered writes improved 52% in the engine and 46% through fio versus v1.2.1. Full-cache close fell from 58.06 ms to 11.66 ms, with 76 backing writes instead of 2,363 and unchanged bytes/barriers. Released as v1.2.2; the measured Rust sources match the release.

## FUTURE WORK

Replace the geometry-sized, fixed-location checkpoint index bodies with inline MappingPage pointers in the checkpoint descriptor and indirect index pages. Allocate index pages in the append log using copy-on-write; reuse unchanged subtrees and publish a new root only after its referenced pages are durable. Reserve a bounded number of indirect root pointers and support enough indirection to cover the format's maximum logical capacity. Four or eight single-indirect pointers alone are insufficient for the current 8 TiB limit.

This connects to [ADR-04: growable thin-provisioned format](ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md), which is proposed and **not implemented**. Mapping snapshots already have variable log locations; the fixed index bodies are the remaining geometry-dependent reservation. The proposed tree would keep bootstrap locations and the log start independent of logical growth, and replace ADR-04's checkpoint arenas and arena relocation. Preserve ADR-04's distinction between logical growth and durable backing provisioning. Settle the combined design in both ADRs and assign a new disk-format version before implementing it. This is separate from v1.2.2's checkpoint I/O batching.
