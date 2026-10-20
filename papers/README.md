# Prior art and design critique

Reviewed: 2026-09-12. Status: research feedback, not an accepted design or implementation plan.

Other storage systems already use this engine's basic approach: append each write to a log, then update a map to its location. The current design keeps an entry for every logical block, rewrites the whole map at each checkpoint, and writes one data block per record. It cannot reuse space from old writes. These choices can cost much more than overwriting data at its existing location. They do not prove that a log is unsuitable for volumes that store changing data, such as databases.

This review uses the documented [V0 engine (on-disk format version 1)](../ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) and [proposed format version 2 (V2)](../ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md). The calculations follow those formats. They are not benchmark results or a code audit. The goal is to understand costs and test alternatives before adding more features. The existing correctness tests remain required, but they do not establish performance at larger sizes.

## Terms used in this review

| Term | Meaning here |
|---|---|
| Logical block address (LBA) | The block number that the filesystem reads or writes. The engine maps it to a physical location in the backing file or device. |
| Payload | The data supplied by a write, excluding the engine's metadata. Each payload block here is 4 KiB. |
| Record and footer | A record contains payload blocks and metadata that describes them. V0 puts that metadata in a footer after the payload. |
| Checkpoint and replay | A checkpoint saves the mapping and recovery position. At startup, replay applies later log records to that saved state. The checkpoint *body* contains the saved map; its *descriptor* identifies and checks the checkpoint. |
| Dense and sparse maps | A dense map has an entry for every addressable block. A sparse map stores entries only for blocks that have a mapping. Neither alone puts a fixed limit on RAM use. |
| Extent | A run of consecutive logical blocks stored at consecutive physical locations. One map entry can describe the whole run. |
| Segment and cleaning | A segment is a region of the log. Cleaning copies its still-needed data elsewhere so the engine can reuse the region. This is also called reclamation or garbage collection. |
| Live data | Data that the current map still references. After an overwrite, the old copy is no longer current. Recovery rules can still require keeping that copy temporarily. |
| Write amplification | Bytes written by a layer divided by payload bytes received by that layer. A ratio of 2× means it writes two bytes for every payload byte. Always name which layer and metadata the count includes. |

## Directly relevant reads

Read the first four in order. Use the remaining papers for the named design question. Each paper supplies a relevant technique or comparison, not a recommendation to reproduce the whole system.

| Paper | Version and source | What to read and borrow |
|---|---|---|
| [Bitcask: A Log-Structured Hash Table for Fast Key/Value Data](2010-bitcask.pdf) | Basho whitepaper, 2010; not a conference paper. [Source PDF](https://riak.com/assets/bitcask-intro.pdf). | All six pages. The closest small engine: append writes, update a key-to-location map in RAM, stop writing to completed files, and merge their current values. Hint files summarize records to speed up startup. Its short recovery description does not fully specify which writes survive a crash. |
| [Logical Disk: A Simple New Approach to Improving File System Performance](1993-logical-disk-technical-report.pdf) | April 1993 technical report. Related conference publication: *The Logical Disk: A New Approach to Improving File Systems*, [SOSP ’93](https://doi.org/10.1145/168619.168621). | Section 2. Put an LBA map below the filesystem, summarize segment contents, and count live bytes to guide cleaning. Its filesystem supplies block lists that ordinary ublk requests do not provide. |
| [The Design and Implementation of a Log-Structured File System](1992-tocs-log-structured-file-system.pdf) | Expanded TOCS ’92 version of the [SOSP ’91 paper](https://doi.org/10.1145/121132.121137). [Source PDF](https://web.stanford.edu/~ouster/cgi-bin/papers/lfs.pdf). | Sections 3.3–3.5 and 4. Identify live records, choose segments to clean, group data with similar lifetimes, and recover from checkpoints. Borrow the cleaning model, not throughput predictions for rotating hard disks. |
| [Beating the I/O Bottleneck: A Case for Log-Structured Virtual Disks](2022-eurosys-log-structured-virtual-disks.pdf) | EuroSys ’22. [Source PDF](https://www.ugurkaynar.com/publications/lsvd-eurosys22.pdf), [publication](https://doi.org/10.1145/3492321.3524271). | Sections 3.1–3.5 and 3.7. Extent maps, records that describe their own contents, replay after a checkpoint, and copying live data to free space. Its remote objects and local cache have different failure assumptions from this engine. |
| [DFTL: A Flash Translation Layer Employing Demand-based Selective Caching of Page-level Address Mappings](2009-asplos-dftl.pdf) | ASPLOS ’09. [Publication](https://doi.org/10.1145/1508244.1508271). | Section 3. Keep the complete map on storage and cache frequently used mappings within a RAM budget. A lookup absent from the cache needs another read. Flash erasure and distributing writes to limit wear are outside this project. |
| [F2FS: A New File System for Flash Storage](2015-fast-f2fs.pdf) | FAST ’15. [Source PDF](https://www.usenix.org/system/files/conference/fast15/fast15-paper-lee.pdf), [publication](https://www.usenix.org/conference/fast15/technical-sessions/presentation/lee). | Sections 2.4–2.7. Separate frequently overwritten data (hot) from rarely overwritten data (cold), and track live blocks. Distinguish a cleaned segment from one safe to reuse after checkpointing. Our block layer cannot directly use classifications that require knowledge of files. |
| [WiscKey: Separating Keys from Values in SSD-conscious Storage](2016-fast-wisckey.pdf) | FAST ’16. [Source PDF](https://www.usenix.org/system/files/conference/fast16/fast16-papers-lu.pdf), [publication](https://www.usenix.org/conference/fast16/technical-sessions/presentation/lu). | Section 3.3.2. Persist copied values, persist their new locations and the marker for space safe to reclaim, then release old space. This ordering applies directly. Its log-structured merge-tree (LSM-tree) index, which stores sorted data in successively merged tables, is not required for our exact LBA lookups. |
| [File System Logging Versus Clustering: A Performance Comparison](1995-usenix-file-system-logging-versus-clustering.pdf) | USENIX ’95. Saved PDF converted from the proceedings PostScript. [Proceedings source](https://www.usenix.org/legacy/publications/library/proceedings/neworl/seltzer.html). | Sections 4–5. Compare database overwrites with cleaning enabled. Test filesystems after repeated use, not only when empty. The hard-disk experiment lost its logging advantage once cleaning ran; it is not a result for modern NVMe solid-state drives (SSDs). |
| [Don’t Stack Your Log on My Log](2014-inflow-dont-stack-your-log-on-my-log.pdf) | INFLOW ’14 workshop, not the OSDI main track. [Source PDF](https://www.usenix.org/system/files/conference/inflow14/inflow14-yang.pdf). | Sections 3–4. Multiple mapping and log layers each add metadata, reserve space, and clean independently. Count extra writes at each layer rather than assuming sequential appends help the complete system. |
| [Efficiently Reclaiming Space in a Log Structured Store](2020-preprint-log-structured-space-reclamation.pdf) | Saved April 2020 arXiv v1 preprint; related publication at [ICDE ’21](https://doi.org/10.1109/ICDE51399.2021.00074). [Preprint source](https://arxiv.org/abs/2005.00044). | Sections 2–3 and 6. Calculate the cost of freeing space. Compare evenly distributed updates with updates concentrated in a few regions. Evaluate cleaning policies using recorded write sequences (traces). Its results are simulations, not measured request times on a live device. |

The PDFs retain their original copyright and license terms. DumpKV was removed from this collection: machine-learned policies for cleaning a value log are a later optimization. Specialized SSD remapping protocols, parallel cleaning, and systems that infer what block contents mean are also outside this reading path.

The pre-existing [Protocol-Aware Recovery for Consensus-Based Storage](Protocol-Aware-Recovery-for-Consensus-Based-Storage.pdf) remains available for the repository's discussion of replication and failures. It is not part of this local mapping and cleaning reading path.

## Design critique and possible improvements

### How can we put a fixed limit on mapping RAM?

Let `A` be the number of addressable blocks and `P` the number of currently mapped blocks. Each block is 4 KiB. A mapping working set is the set of map entries a workload uses repeatedly.

| Design | What determines its size? | Main limit |
|---|---|---|
| V0 dense arrays | `12 × A` bytes in RAM. | Advertising mostly empty capacity still consumes memory. |
| V2 packed sparse checkpoint body | `16 × P` bytes per body, rounded to 4 KiB. | This is a disk size, not the RAM cost or the space reserved for two checkpoints. Hash maps, trees, and sorting can need additional RAM. |
| Extent map | Number of consecutive logical-to-physical runs. | Random overwrites split runs. The map can end up needing nearly one entry per block. |
| Map stored on disk with a fixed-size cache | Cache budget, plus any directory kept in RAM to locate mapping pages. | Missing cache entries require reads. Evicting a changed page requires safe persistence and recovery rules. |

V0 needs 3 GiB per TiB of logical capacity in RAM and in each checkpoint body. V2 needs 4 GiB in one packed checkpoint body per TiB of mapped data. Its checkpoint arena (the reserved storage region) holds two slots sized by `checkpoint_entry_capacity`. This capacity can exceed the current number of mapped blocks. At capacity for 1 TiB of mapped data, the two bodies reserve 8 GiB, plus arena metadata.

The two formats have equal checkpoint-body sizes when 75% of logical blocks are mapped. Above that fraction, V2's body is larger. Its sparse RAM map may become larger than V0's arrays at a lower fraction.

A discard request tells the device that a logical range is no longer needed. Without discard, the mapped-block count does not decrease when a filesystem or database frees data internally.

The checksum uses 8 of V0's 12 bytes per block. Representing addresses as extents does not automatically compress the checksums. A smaller map must still account for where validation metadata lives. Moving checksums out of RAM is different from removing integrity checks.

**Recommendation:** keep the dense map as a simple baseline. Measure a sparse map before choosing it as the replacement. If writes use only a few logical regions, allocating fixed-size mapping pages only for those regions is another option. If the populated map still exceeds the RAM budget, evaluate storing it on disk with a fixed-size page cache, as DFTL does. Use extents only if write traces show that useful consecutive runs remain after overwrites.

Count fixed overhead per volume separately. A thousand mostly idle volumes can exhaust RAM through queues, buffers, and runtime state even when their maps are small. These papers do not establish that thousands of ublk devices fit within the host's RAM budget.

### Why are writes expensive even before cleaning?

V0 writes one 4 KiB footer for each 4 KiB payload: 2× log-write amplification before checkpoints or cleaning. The format permits up to 338 payload blocks per footer. A full batch would reduce that ratio to `339 / 338`, approximately 1.003×. This requires enough writes to combine. Frequent flush requests can force smaller batches because earlier writes must become durable without waiting for later writes.

By default, V0.6 also writes the complete map after at most 64 MiB of physical log writes. With one-block records, that is only 32 MiB of payload between checkpoints.

| Logical capacity of one volume | Full checkpoint body | Log plus checkpoint-body bytes / payload bytes |
|---:|---:|---:|
| 1 GiB | 3 MiB | 2.094× |
| 100 GiB | 300 MiB | 11.375× |
| 1 TiB | 3 GiB | 98× |

These ratios are calculated over a full checkpoint interval. They exclude descriptor writes, extra checkpoints on close, writes by the underlying filesystem, and writes inside the SSD. They are not latency measurements. Many small volumes do not have the same checkpoint ratio as one large volume with the same total capacity.

For payload bytes `D`, log bytes `L`, and one checkpoint body `M`, the ratio is `(L + M) / D`. A larger batch reduces `L / D`, but does not remove `M`. A sparse checkpoint omits empty entries but still rewrites every populated entry.

**Recommendation:** set a checkpoint-write budget and a recovery-time target together. A longer interval reduces checkpoint writes but increases the log that recovery must replay. If full-map checkpoints cannot meet both targets, evaluate these alternatives:

- Copy-on-write mapping pages: write changed pages to new locations, then safely publish the map that references them.
- Checkpoint deltas: save changes since an earlier checkpoint, with a limit on how many changes recovery must apply. Periodically write a full checkpoint.

Writing only changed pages into the existing alternating checkpoint bodies is not automatically safe. Each body must describe one complete checkpoint version. For example, the inactive body may lack changes saved only in the active body.

ADR-04 allows the engine to relocate checkpoint arenas as they grow and reuse their slots for later checkpoints. This avoids consuming new log space for every checkpoint. It does not reduce the bytes written per checkpoint, the cost of sorting entries, or write pauses. Resolve those costs before choosing this as the final mapping design. Recovery also has to load and validate the checkpoint, not just replay later records.

Batching must preserve write order and flush guarantees. Format version 1 forbids duplicate LBAs within one record. Combining queued writes therefore needs an explicit rule for repeated writes to the same LBA.

### Why do repeated overwrites eventually fill the device?

Every overwrite appends a new record. V0 never reuses the old record's space, even when no current mapping needs it. Repeatedly overwriting just one LBA therefore fills the finite log. Sequential writes to new locations do not, by themselves, establish a long-term performance advantage.

ADR-04 proposes pausing writes and copying all mapped blocks to a replacement image. This is offline compaction: it removes obsolete copies while the volume is not accepting writes. It is a simple baseline, but it needs space for the replacement image. The pause grows with the amount of live data copied. It does not by itself support a database that must keep accepting writes.

**Recommendation:** keep replacement-image copying as an offline operation or a reference for correctness tests. To support continuous writes, first compare cleaning policies in a small simulation of write sequences. Divide the log into segments and stop appending to each segment when it is full (seal it). Track each segment's live-byte count. Use the current LBA map to determine which records are still needed.

Start with two policies: clean the oldest sealed segment first (first in, first out, or FIFO), and clean the segment with the fewest live bytes first (greedy selection). Separating frequently overwritten data from rarely overwritten data requires separate write streams. Add that only if measurements show that it avoids enough copying to justify the complexity.

Cleaning while application writes continue needs more than a rule for choosing segments:

- Copy only records that the current LBA map still references. Before changing the map, confirm it still points to the source record. Otherwise, a cleaner could replace a concurrent write with an older value.
- Persist the copied data and the mapping changes needed to recover it before reusing source space. Keep that space while any checkpoint that recovery may select still needs it. Updating only the newest checkpoint pointer is insufficient when V2 permits falling back to an older checkpoint.
- Reserve space for copying live data and writing metadata. Delay or reject application writes before they consume this reserve. Otherwise, the engine can run out of the space it needs to free more space.
- Assign a new identity to each reused segment so recovery can reject records left from its previous use. Verify source checksums before copying. Recompute checksums that include the physical location when the data moves.

The copying cost rises sharply when selected segments contain mostly live data. Let `u` be the fraction of bytes still live in the segments selected for cleaning. For a simplified model where cleaning and writes continue at stable rates, excluding headers and metadata:

```text
bytes copied / new payload bytes = u / (1 - u)
total data bytes written / new payload bytes = 1 / (1 - u)
```

For example, cleaning a 100 MiB segment that is 80% live copies 80 MiB and frees a net 20 MiB. Accepting 20 MiB of new payload then writes 100 MiB in total: 5× write amplification.

Segments that are 50%, 80%, and 95% live imply 2×, 5×, and 20× amplification, respectively. These fractions describe the selected segments, not how full the whole device is. The original log-structured file system (LFS) paper's `2 / (1 - u)` cost also counts reading whole segments. It is not directly comparable to a count of writes alone.

Discard must be an explicit operation, not something inferred from a block of zeros. Filesystem deletion and database record deletion do not necessarily issue discard requests. A future implementation must define ordering, persistence, and what reads return after discard. Replay and cleaning must not restore discarded data.

### What benefit justifies the extra mapping layer?

The Fast File System (FFS) and ext4 normally overwrite already allocated file data without allocating another filesystem data block. They store their metadata on disk and cache it in RAM. They do not require this engine's additional RAM entry for each LBA. They have other costs: metadata updates, fragmentation, and, for ext4, journaling (logging changes for recovery). The SSD underneath them may still relocate every write internally.

The USENIX ’95 comparison found LFS about 50% faster than FFS without cleaning in its transaction workload. At 48% filesystem utilization, cleaning reduced LFS throughput by 34% relative to that no-cleaning result, eliminating the advantage. This is evidence against benchmarking only an empty log, not proof that modern log-structured systems lose. F2FS and Log-Structured Virtual Disks (LSVD) provide counterexamples under the conditions they evaluated.

**Recommendation:** identify a concrete benefit from this layer, such as isolating thin-provisioned volumes, controlling data placement, checking integrity, or reducing measured I/O. Thin provisioning means allocating physical storage as logical blocks are used. It does not require a log. [Linux dm-thin](https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/thin-provisioning.html) provides a useful comparison with an existing implementation, not another engine to reproduce.

Do not choose a shared log solely because there are many volumes. A shared log can combine writes into larger batches and share free space. It also links the volumes' recovery, cleaning, and exposure to failures. Separate logs simplify isolation and deleting a whole volume. Measure these trade-offs before introducing a shared storage pool.

## Proposed on-disk changes for online reclamation

Status: proposal for discussion, not an accepted format. [ADR-04](../ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md) says that "V2 does not define segment reuse". Its last open question asks when replacement compaction stops being enough and segmented online cleaning is needed. This section writes down the on-disk changes that cleaning needs, so we can judge the cost before drafting a compaction ADR.

### This is segment cleaning, not LSM merging

An LSM tree merges its files because it stores keys in sorted runs, and one lookup may have to check every run. Merging keeps that read cost bounded.

This engine does not have that problem. It keeps an exact map from LBA to physical block, so one read touches one record. There is nothing to merge.

The mechanism we need is segment cleaning, taken from LFS, F2FS and flash translation layers. It works like this:

1. Split the physical log into fixed-size segments.
2. Count how many blocks in each segment are still live.
3. Pick a segment with few live blocks.
4. Copy those live blocks to the head of the log.
5. Mark the segment free and write to it again.

One rule from the LSM world still applies: write the copies, then write the new map, then release the old space. WiscKey section 3.3.2 states it.

### What V2 already does, and what it leaves open

Most of V2 grows the volume. Very little of it reuses space.

| V2 mechanism | What it does | What it leaves open |
|---|---|---|
| Fixed roots, arena generation | Boots without reading a size-dependent layout, and moves metadata safely. | Frees no payload blocks. |
| `GROW_LOGICAL` | Grows the size the user sees. | Adds no physical space. |
| `PROVISION_BACKING` | Claims more physical space. | Delays the problem. It takes more medium instead of reusing the medium it holds. |
| `CHECKPOINT_ARENA` | Grows checkpoint capacity. | Strands the old arena until a full-image compaction runs. |
| Replacement-image compaction | Frees space offline, with a short crash argument. | Needs a pause, a second image, and an external owner to swap them. |

When the log fills, V2 grows. That works while the medium can grow and the volume can go offline to compact. It is not a way to run sustained overwrites.

One boundary matters for scoping:

**Cleaning frees physical blocks. It does not reduce `mapped_entries`.**

Checkpoint write volume, arena capacity and resident RAM all follow `mapped_entries`. That number drops only when a discard path removes mappings. The RAM and checkpoint costs described earlier in this document are therefore separate work, and a cleaning ADR should not claim to fix them.

### The on-disk changes

Five changes. Segments cover only the data-log region. Checkpoint arenas stay outside it and keep V2's existing rules.

**1. Store the segment size in FORMAT.**

Add an immutable `segment_blocks` field. The log region runs from the first log block to `provisioned_backing_blocks` and splits into segments of that size. Arenas are not part of the split.

**2. Put a header block at the start of each segment.**

It holds the volume ID, the segment index, a `segment_epoch`, and the LSN at which the segment was opened. The epoch rises by one every time the segment is reused.

The epoch is not needed to stop replay. V2 already checks the record LSN and the previous-record link. A record left over from the segment's earlier life carries an old LSN and a back-link that does not match, so replay stops there, exactly as it stops at a stale tail today.

The epoch is needed for three other things: an offline salvage tool, which scans blocks without that chain context; telling a reused region apart from a corrupt one under ADR-05's policy; rebuilding segment state cheaply at startup.

ADR-04 already asks for this, as "recycle that segment with a new generation".

**3. Write a summary at the end of each segment.**

When a segment is sealed, write its reverse map, physical block to LBA, into the last blocks of the segment. It needs `ceil(segment_blocks * 4 / 4096)` blocks.

At 64 MiB segments that is 16 blocks out of 16384, or 0.10%. Without it, the cleaner reads one record header every few blocks to learn which LBAs a segment holds. With it, the cleaner does one sequential read.

**4. Add a segment table to the checkpoint body.**

The body becomes the segment table followed by the existing sorted mapping entries, with one checksum over both. The descriptor field at offset 60, currently zero, becomes `segment_table_blocks`.

Each segment gets a 16-byte entry holding its epoch and its state: free, open, sealed, cleaning or reclaimable. At 64 MiB segments over 1 TiB provisioned that is 16384 entries, 64 blocks, 256 KiB per checkpoint.

Do not store live-block counts. Derive them at startup by counting how many recovered mapping entries fall in each segment. Then keep them current on the write path: when a write moves LBA `L`, subtract one from the segment holding its old physical block and add one to the segment holding the new one. One array decrement per write pays for victim selection.

**5. Change two validation rules.**

V2 rejects a mapping entry whose physical block sits at or after the checkpoint's append block. Once segments are reused, a block's position no longer tells you its age, so that test breaks. Replace it: the block must fall inside the log region, and its segment must not be free.

V2 also finds the next record at `header + 1 + payload_blocks`. That still holds inside a segment. It does not hold across a segment boundary. So recovery stores an append position as a segment plus an offset, and replays segments in the order given by their header's open LSN.

### When a segment may be reused

ADR-04 already gives the rule:

> Never overwrite or reclaim a physical payload while either valid root can select a checkpoint that references it.

There are two roots and they alternate, so a segment is safe to reuse only after both have been rewritten without it. That gives this order:

```text
1. copy the live blocks out as normal WRITE records, then fsync
2. update the in-memory map
3. write a checkpoint into the inactive slot   -> one root is now clean
4. write a checkpoint into the other slot      -> both roots are now clean
5. mark the segment free and raise its epoch
```

Between steps 2 and 4 the segment is `reclaimable` and the allocator must not hand it out. Each release costs two checkpoints, so clean several segments per cycle and release them together.

Two more rules follow from the format as it stands.

A record is live only if `mapping[lba]` holds that exact physical block. Checking that the block sits inside the victim segment is not enough. If a write lands on that LBA while the cleaner runs, the cleaner must not overwrite it with the older copy it is carrying.

Payload checksums are seeded with the physical block, so a cleaner that moves a block must verify it with the old address and recompute it with the new one. That is CPU work, not a reason to drop the physical block from the seed. Keeping it is what detects a misdirected write.

Keep the single serialized writer, and submit the cleaner's copies as ordinary WRITE records through it. Then there is no cleaner-versus-writer race to get right. Give the cleaner its own thread later, if a latency measurement says you must.

### Free space and backpressure

The cleaner needs somewhere to write. Reserve at least two segments, one for foreground writes and one for the cleaner's copies, and stall foreground writes above that reserve.

The device advertises a fixed size and cannot return `ENOSPC`, so stalling is the only correct behaviour when space runs out. This means the cleaner must make progress while every foreground write is blocked.

Giving the cleaner its own segment is also the cheapest useful placement rule. A block that survived cleaning is colder than a fresh write, almost by definition, and mixing the two is what leaves segments stuck at half live. Do this before trying to guess which LBAs are hot. An opaque block layer cannot know.

Build victim selection in this order. FIFO first, because it is correct and small. Then greedy, picking the segment with the fewest live blocks. Then cost-benefit, but only if a trace shows greedy copying the same cold data again and again.

### Two open questions

**How big should a segment be?**

`segment_blocks` is fixed when the image is formatted and never changes. But a volume can start at 128 MiB and reach 8 TiB, and no single value covers both ends:

- At 64 MiB, a 128 MiB volume holds 2 segments. Cleaning needs 2 free segments to work, so that volume can never clean.
- At 4 MiB, an 8 TiB volume holds 2 million segments. The segment table reaches 32 MiB, written in every checkpoint.

There are two ways out.

Accept the limit. Pick a large segment size. A small volume then never cleans: when it fills it calls `PROVISION_BACKING` and grows. V2 already behaves this way, so this costs nothing new. This is the recommended option.

Or let the size change. Add a record that sets a new `segment_blocks`. A region cannot be re-cut into different-sized segments while data sits in it, so every live block must be copied out first. That copy costs as much as the offline compaction V2 already has.

**When should the engine grow, and when should it clean?**

Both answer the same signal: free space is low. V2 has only the growth answer. The obvious policy is a threshold on the live fraction, cleaning when the log holds enough dead blocks to be worth recovering and growing when it does not. Where that threshold sits depends on the price of medium against the write amplification of copying. Take it from the trace model, not from a guess.

**Recommendation:** build the metadata-only trace model described in the next section before writing any of this format. It needs no format change and no I/O, and it tells you the segment size, the reserve, and whether greedy selection is enough. Review the on-disk changes above against ADR-04's reuse rule now, but do not implement them until those numbers exist.

## Small experiments that can change the design

These are proposed investigations, not completed tests or new acceptance requirements. They do not replace the current filesystem correctness work.

| Question | Small experiment | Evidence to collect |
|---|---|---|
| What determines mapping RAM? | Keep logical capacity fixed. Vary how many blocks are mapped and whether their addresses are consecutive or scattered. After an initial sequential fill, compare evenly distributed overwrites with writes concentrated in a small region. Also move that region over time. | Allocated bytes, peak resident RAM (RSS), bytes per mapped block, extent count, and lookup time. Measure sorting RAM and checkpoint-load time. Count idle per-device overhead separately. |
| Can checkpoints meet the write and recovery budgets? | Count payload, footer, and checkpoint bytes separately. Vary logical size, batch size, and checkpoint interval on disposable files. | Bytes written per payload byte, by category; write pauses; flush latency; checkpoint-load time; replay time. Check the counts against the formula before interpreting throughput. |
| When does cleaning become expensive? | Simulate mappings and writes without storing payloads. Fix physical capacity. Compare FIFO and greedy selection with evenly distributed overwrites and writes concentrated in a fixed or moving region. Vary how full the device is. Continue beyond the initial fill until the measurements stabilize. | Live bytes copied, live fraction of selected segments, reserved free space, and write amplification over time. This model cannot establish real I/O latency or crash safety. |
| Does the complete system benefit? | After correctness tests pass, compare the same filesystem and database settings with and without the engine. Test overwrites and deletion followed by reuse. Report initial filling separately from operation with cleaning. | Throughput, 99th-percentile request latency (p99), actual discard requests, observable bytes written at each layer, RAM, and recovery correctness. Report the backing layers, cache state, when the database calls `fsync`, and differences in integrity checks. |

Use only explicitly disposable backing files or approved disposable devices. A ublk device backed by a regular file includes an underlying filesystem that a raw-device comparison does not. Label that difference. Engine write counters do not measure writes to the SSD's flash memory. Until reclamation exists, report finite-log capacity and time to exhaustion, not sustained database throughput.

Start with mapping and checkpoint accounting. They can expose a failed assumption without adding a cleaner, changing the persistent format, or running a large database benchmark.

## Ideas to improve the design

Status: proposal for discussion, 2026-09-15. These are two separate changes: page the mapping to reduce RAM and checkpoint work; segment the log to reuse obsolete records. Neither requires implementing ADR-04's growth proposal first.

Start with fixed logical and backing sizes, one serialized writer, and a new format without in-place conversion. Keep the two-checkpoint fallback contract. The structures below describe the proposed contents and ownership; exact record offsets and encodings belong in the implementation design.

### 1. Page the mapping and checkpoint only changed pages

Use copy-on-write mapping pages and a flat page index. No extent tree, separate mapping journal, or multilevel index initially.

LFS sections 3.1 and 4.1 provide the useful pattern: persist modified mapping blocks, then publish their locations through a checkpoint. F2FS sections 2.1 and 2.7 provide another approach, versioned metadata blocks selected by checkpoint bitmaps. We choose copy-on-write pages here because the engine already appends records.

LSVD section 3.3 still checkpoints full extent maps. Its cleaning design is relevant, but it does not supply this incremental mapping scheme.

#### What changes on disk and in RAM?

Each mapping page covers 320 consecutive LBAs. The page number and entry position imply the LBA:

```text
page_id = LBA / 320
entry   = LBA % 320

MappingPage record (one 4 KiB block)
  record identity: kind, format version, volume ID, LSN
  page_id                         u32
  entries[320]
    physical_payload_block        u32
    payload_checksum              u64
  zero padding and record checksum

PageIndex
  mapping_page_block[page_count]   u32
  page_count = ceil(logical_blocks / 320)
```

The entries occupy 3840 bytes, leaving 256 bytes for framing, padding and the record checksum. Entries beyond the logical volume size in the final page must be zero. Physical block zero means unmapped. A zero page-index entry means that the entire logical range is unmapped. Checksums stay with their mappings, on disk and in cached pages; payload verification is unchanged.

Replace each checkpoint's full mapping body with a complete `PageIndex`. Keep two fixed slots, each containing a descriptor and its own index body. The descriptor identifies the generation, body length/checksum and replay position. Slots remain outside the append log and are sized at format time.

```text
Disk: checkpoint A or B → page index → mapping pages → payloads

RAM: Volume
  backing I/O and volume geometry
  current page index
  bounded mapping-page cache, including dirty pages
  append, durability and checkpoint cursors
  active checkpoint slot and generation
```

The two dense mapping arrays disappear. The flat index stays resident; mapping pages load on demand. A read whose page is absent from the cache needs a metadata read before its payload read. Bound the cache, buffers and recovery working memory explicitly.

At 1 TiB logical capacity, there are 838,861 pages (?). Four bytes per page pointer gives a 3,355,444-byte index (about 3.2 MiB), compared with the current 3 GiB checkpoint body. At 8 TiB there are 6,710,887 pages and the index is 26,843,548 bytes (about 25.6 MiB). These are pointer-array sizes, excluding descriptors, padding and cache memory. We still rewrite the whole index; page the index itself only if that cost becomes unacceptable.

#### How do writes and checkpoints work?

A data write follows the existing publication rule: append the data record, require exact completion, then update its cached mapping entry. Mark the page dirty. Do not append a mapping page for every write.

Checkpoint publication stops new writes and performs this sequence:

```text
append dirty mapping pages; update a candidate page index
→ fsync data and mapping-page records
→ write the inactive checkpoint's complete index body
→ fsync the body
→ write its descriptor with the new generation and replay position
→ fsync the descriptor; select the checkpoint; resume writes
```

Unchanged pages keep their old pointers. For example, if only LBA 42 changes, checkpointing writes a new page 0 and a new index. It does not rewrite the other mapping pages. The older checkpoint continues to reference the previous page 0.

Use cache pressure as another checkpoint trigger rather than adding independent dirty-page spill machinery. Keep the replay-tail bound compatible with the recovery memory budget, including recovery from the older checkpoint.

Recovery loads a valid checkpoint index and replays subsequent data records to update mappings. Mapping-page records are snapshots, not new logical writes; an uncommitted snapshot must not independently replace recovered state. The new record framing must let recovery distinguish and traverse both kinds. A successful data flush remains recoverable without requiring a checkpoint.

This first change keeps the log append-only. Old data and mapping-page records still consume space. It improves mapping RAM and checkpoint I/O, but does not yet provide sustained overwrites.

### 2. Segment the log and add a serialized cleaner

Keep the paged mapping design independent of segmentation. When combined, the cleaner must relocate live mapping pages as well as payloads. Otherwise, metadata eventually fills the device even if data cleaning works.

Start with greedy victim selection: choose the eligible sealed segment with the fewest live blocks. F2FS section 2.5 uses greedy selection for foreground cleaning; LSVD section 3.5 uses it for object cleaning. Defer cost-benefit selection, hot/cold streams and a concurrent cleaner thread.

#### What changes on disk and in RAM?

Divide the append region into fixed-size segments. Keep both checkpoint slots in a reserved prefix, outside all segments. Each slot now holds the page index followed by a flat segment table, covered by the checkpoint checksum. The checkpoint generation selects both together; no separate segment-metadata tree is needed initially.

```text
Disk
  checkpoint A: descriptor + page index + segment table
  checkpoint B: descriptor + page index + segment table
  segment 0: header | data and mapping-page records | summary
  segment 1: header | data and mapping-page records | summary
  ...

Segment header
  volume ID, segment ID, reuse generation, opening LSN, checksum

Segment summary
  record locations and kinds
  payload locations and LBAs; mapping-page locations and page IDs
  used length, segment identity and checksum

Checkpoint segment table
  one entry per segment: reuse generation and state
  states: free, open, sealed

Additional RAM state
  segment table and live-block counts
  append position: segment ID + offset
  current segment's summary buffer
  cleaner victim and copy cursor
  segments awaiting safe release
```

Records do not cross segment boundaries. Reserve summary space before accepting records. Seal the segment by persisting its summary; recovery scans record framing for an interrupted or still-open segment. Reopening a reused segment writes a new generation before appending its records. Record identity and replay ordering must reject bytes from an earlier use, including crashes during opening and sealing.

The current map decides whether a payload is live. The current page index decides whether a mapping-page record is live. Summaries identify candidates, not authority. Reconstruct live counts at startup by streaming mapping pages and the page index, then maintain them on updates. This avoids another persistent counter protocol, but startup still reads the populated mapping; it is not constant-time recovery.

Choose segment size from physical backing capacity, not logical size. A 128 MiB logical device can have much more backing. Start experiments with 32 MiB segments, an unmeasured initial choice, and compare 16 and 64 MiB if copying pauses or reserve requirements justify it. Keep the size fixed for an image. Reserve checkpoint space first and use only complete segments in the remaining region.

#### How does background cleaning stay simple?

Run cleaning in bounded batches between foreground requests, through the same serialized writer. Foreground operations can run between batches, but not between checking a source mapping and publishing its relocated copy. This is background scheduling, not concurrent ownership of the map.

For each victim, check exact source addresses, verify payloads, and append live copies through the normal write path. Recompute location-bound checksums. Relocate mapping pages still selected by the current index. If a page is dirty, write its current cached contents rather than restoring its old on-disk mappings. Use the same append head for foreground data, cleaner copies and metadata.

The barrier is checkpoint publication before release:

```text
copy live payloads and mapping pages out of the victim
→ persist the resulting mapping state
→ publish a checkpoint that no longer depends on the victim
→ publish the other checkpoint without that dependency
→ allow the allocator to reuse the victim
```

Both publications include the segment's release in their segment tables. Until both are durable, keep the victim pending in RAM and unavailable for allocation. The free state in just the newest checkpoint is not sufficient permission to overwrite it. After a crash, recovery must reconcile the surviving checkpoint views and complete any required publication before reuse.

Protect every recovery dependency, not only current payloads: older mapping pages and record headers needed to resume replay can also pin a segment. A candidate is releasable only when neither usable checkpoint nor its required replay path needs it. The single writer also ensures that an in-flight read cannot overlap physical reuse.

For the first implementation, release one victim per cycle. Two checkpoint publications per victim are an accepted initial cost; grouping victims can follow if measurement shows this dominates. A failed copy or publication never authorizes release.

#### When should cleaning run?

Try background cleaning when obsolete record bytes exceed 33% of occupied record bytes in sealed segments. Count both data and mapping-page records. This is an experimental policy, not a proven crossover. Obsolete means not selected by current mappings; those bytes may still be protected by an older checkpoint.

Use free-space pressure independently. When immediately reusable space falls below a low watermark, prioritize cleaning over new writes until a higher watermark is reached. Idle-only cleaning cannot keep up with a continuously busy device.

Reserve enough physical space to complete a victim's relocation and mapping-page writes, including record framing and segment overhead. Checkpoint slots are already reserved separately. Compute this requirement before admitting foreground writes; a guessed count of two segments is not a progress proof. If no victim can produce net free space and the fixed backing cannot satisfy a write, return an explicit capacity error rather than stall forever.

### Implement and test the two changes separately

Paged mapping is the smaller first change because it preserves old bytes. Segmentation and cleaning add destructive reuse, allocation recovery and metadata relocation.

1. Implement mapping-page records, the flat index and checkpoint replacement on the finite log. Test sparse reads, cache pressure, unchanged-page reuse, data flush without checkpoint, and fallback after interrupted publication. Measure index/page writes, RAM and recovery reads.
2. Add segments and the serialized greedy cleaner. Test interruption during opening, sealing, copying and both release checkpoints. Include metadata-only victims and reuse followed by fallback recovery. Run repeated overwrites well beyond backing capacity and verify data after restart.

Build the necessary fault injection with these persistence transitions. Completing the broader ADR-05 medium-fault project is not a prerequisite for designing them. Use disposable files and extend the existing recovery tests rather than starting a separate simulation framework.

[The performance plan](../PLAN-SQLITE-EXT4-PERFORMANCE.md) remains a separate workstream for larger requests, record batching and cheaper checkpoint I/O. Neither finishing that plan nor implementing growth is a prerequisite for these design changes.
