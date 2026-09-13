# Prior art and design critique

Reviewed: 2026-09-12. Status: research feedback, not an accepted design or implementation plan.

The append-log plus logical-block-address (LBA) map has direct precedents. The current design's limits are dense metadata, full-map checkpoints, one-block records, and no space reuse. Those limits can make it substantially more expensive than an update-in-place block path. They do not establish that log structuring is unsuitable for state volumes.

This review uses the documented [V0 engine (on-disk format version 1)](../ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) and [proposed format version 2 (V2)](../ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md). Calculations below follow those formats; they are not benchmark results or a code audit. The learning goal is to derive costs, identify assumptions, and test alternatives before expanding the engine. The existing correctness gates remain prerequisites, not evidence of performance at scale.

## Directly relevant reads

Read the first four in order. Use the remaining papers for the named design question. These papers supply mechanisms or comparisons directly applicable to this engine; inclusion is not a recommendation to reproduce each system.

| Paper | Version and source | What to read and borrow |
|---|---|---|
| [Bitcask: A Log-Structured Hash Table for Fast Key/Value Data](2010-bitcask.pdf) | Basho whitepaper, 2010; not a conference paper. [Source PDF](https://riak.com/assets/bitcask-intro.pdf). | All six pages. The closest small engine: append, update an in-memory key-to-location map, seal files, merge current values, and load hint files at startup. Its short recovery description is not a complete durability specification. |
| [Logical Disk: A Simple New Approach to Improving File System Performance](1993-logical-disk-technical-report.pdf) | April 1993 technical report. Related conference publication: *The Logical Disk: A New Approach to Improving File Systems*, [SOSP ’93](https://doi.org/10.1145/168619.168621). | Section 2. An LBA map below the filesystem, segment summaries, and live-byte accounting for cleaning. Its filesystem-supplied block lists are not available through ordinary ublk requests. |
| [The Design and Implementation of a Log-Structured File System](1992-tocs-log-structured-file-system.pdf) | Expanded TOCS ’92 version of the [SOSP ’91 paper](https://doi.org/10.1145/121132.121137). [Source PDF](https://web.stanford.edu/~ouster/cgi-bin/papers/lfs.pdf). | Sections 3.3–3.5 and 4. Identify live records, select victims, group similar lifetimes, and recover checkpoints. Borrow the cleaning model, not HDD throughput predictions. |
| [Beating the I/O Bottleneck: A Case for Log-Structured Virtual Disks](2022-eurosys-log-structured-virtual-disks.pdf) | EuroSys ’22. [Source PDF](https://www.ugurkaynar.com/publications/lsvd-eurosys22.pdf), [publication](https://doi.org/10.1145/3492321.3524271). | Sections 3.1–3.5 and 3.7. Extent maps, self-describing records, checkpoint replay, and live-data collection. Its remote objects and local cache have different failure assumptions from this engine. |
| [DFTL: A Flash Translation Layer Employing Demand-based Selective Caching of Page-level Address Mappings](2009-asplos-dftl.pdf) | ASPLOS ’09. [Publication](https://doi.org/10.1145/1508244.1508271). | Section 3. Persist the complete map and cache active mappings under a RAM budget. Mapping misses add I/O. NAND-specific erase and wear-leveling machinery is outside this project. |
| [F2FS: A New File System for Flash Storage](2015-fast-f2fs.pdf) | FAST ’15. [Source PDF](https://www.usenix.org/system/files/conference/fast15/fast15-paper-lee.pdf), [publication](https://www.usenix.org/conference/fast15/technical-sessions/presentation/lee). | Sections 2.4–2.7. Hot/cold placement, live-block tracking, and the distinction between a cleaned segment and one safe to reuse after checkpointing. Do not copy filesystem-specific classification into an opaque block layer. |
| [WiscKey: Separating Keys from Values in SSD-conscious Storage](2016-fast-wisckey.pdf) | FAST ’16. [Source PDF](https://www.usenix.org/system/files/conference/fast16/fast16-papers-lu.pdf), [publication](https://www.usenix.org/conference/fast16/technical-sessions/presentation/lu). | Section 3.3.2. Persist relocated values, persist new pointers and the reclaim boundary, then release old space. This ordering applies directly; the LSM-tree index is not required for our exact LBA lookups. |
| [File System Logging Versus Clustering: A Performance Comparison](1995-usenix-file-system-logging-versus-clustering.pdf) | USENIX ’95. Saved PDF converted from the proceedings PostScript. [Proceedings source](https://www.usenix.org/legacy/publications/library/proceedings/neworl/seltzer.html). | Sections 4–5. Compare database overwrites with cleaning enabled, and compare against aged rather than only empty filesystems. The HDD experiment lost its LFS advantage once cleaning ran; it is not a modern NVMe result. |
| [Don’t Stack Your Log on My Log](2014-inflow-dont-stack-your-log-on-my-log.pdf) | INFLOW ’14 workshop, not the OSDI main track. [Source PDF](https://www.usenix.org/system/files/conference/inflow14/inflow14-yang.pdf). | Sections 3–4. Multiple mapping/log layers add metadata, reserve space, and uncoordinated cleaning. Measure amplification at each boundary rather than assuming sequential appends benefit the complete stack. |
| [Efficiently Reclaiming Space in a Log Structured Store](2020-preprint-log-structured-space-reclamation.pdf) | Saved April 2020 arXiv v1 preprint; related publication at [ICDE ’21](https://doi.org/10.1109/ICDE51399.2021.00074). [Preprint source](https://arxiv.org/abs/2005.00044). | Sections 2–3 and 6. Derive reclamation cost, compare uniform and skewed updates, then evaluate cleaners with traces. Its results are simulations, not measured latency on a live device. |

The PDFs retain their original copyright and license terms. DumpKV was removed from this collection: learned LSM value-log policies are a second-order optimization here. Specialized SSD remapping protocols, parallel GC frameworks, and semantic inference systems are also outside this reading path.

The pre-existing [Protocol-Aware Recovery for Consensus-Based Storage](Protocol-Aware-Recovery-for-Consensus-Based-Storage.pdf) remains available for the repository's replication/fault-model discussion. It is not part of this local mapping-and-cleaning reading path.

## Design critique and possible improvements

### RAM: sparse is not the same as bounded

Let `A` be addressable blocks, `P` currently mapped blocks, and `W` the mapping working set. All blocks below are 4 KiB.

| Design | Space follows | Failure boundary |
|---|---|---|
| V0 dense arrays | `12 × A` bytes in RAM | Advertising mostly empty capacity still consumes memory. |
| V2 packed sparse checkpoint body | `16 × P` bytes per body, rounded to 4 KiB | Not the RAM size of the proposed sparse map or the two-slot arena reservation. Hash/tree overhead and checkpoint sorting need measurement. |
| Extent mapping | Number of contiguous logical-to-physical runs | Random overwrites split runs. An initially compact map can approach per-block mapping. |
| Persistent mapping with a bounded cache | Explicit cache budget, plus resident directory metadata | Cache misses add mapping reads; dirty eviction adds metadata writes and recovery obligations. |

V0 needs 3 GiB per TiB of logical capacity in RAM and in each checkpoint body. V2 needs 4 GiB in one packed checkpoint body per TiB of mapped data. Its arena reserves two slots sized by `checkpoint_entry_capacity`, which can exceed the mapped population. At capacity for 1 TiB of mapped data, the two bodies reserve 8 GiB, plus arena metadata.

The packed checkpoint-body sizes cross at 75% occupancy; the in-memory sparse representation may become larger earlier. Without discard, mapped population does not decrease when a filesystem or database frees data internally.

The checksum is 8 of V0's 12 bytes per block. Compressing physical addresses into extents does not compress those checksums automatically. Any RAM reduction must account for both address and validation metadata; removing integrity checking is not an equivalent optimization.

**Recommendation:** retain the dense representation as a simple comparison point. Measure a sparse representation before accepting it as the scalable replacement. If partial population is clustered, sparsely allocated fixed-size mapping pages are another candidate. If populated metadata still exceeds the budget, evaluate a persistent paged map with bounded caching, following DFTL. Use extent maps only if traces retain useful contiguous runs after overwrites.

Budget per-volume fixed overhead separately. A thousand mostly idle volumes can exhaust memory through queues, buffers, and runtime state even when their maps are small. The papers do not prove that thousands of ublk devices fit the host budget.

### Checkpoints and record framing: costs exist before GC

V0 writes one 4 KiB footer per 4 KiB payload: 2× immediate log-write amplification. The format permits 338 payload blocks per footer, reducing this framing ratio to `339 / 338`, approximately 1.003×. Achieving that ratio requires real batching opportunities; frequent flushes can keep batches small.

V0.6 also writes the complete mapping body after at most 64 MiB of physical log writes by default. With one-block records, that represents 32 MiB of block payloads between checkpoints.

| Per-volume logical capacity | Full checkpoint body | Log plus checkpoint-body bytes / payload bytes |
|---:|---:|---:|
| 1 GiB | 3 MiB | 2.094× |
| 100 GiB | 300 MiB | 11.375× |
| 1 TiB | 3 GiB | 98× |

These are amortized format calculations at the documented interval. They exclude descriptor writes, extra checkpoints on close, underlying filesystem traffic, and SSD-internal amplification. They are not latency measurements. Many small independent volumes do not have the same checkpoint ratio as one large volume with the same aggregate capacity.

For payload bytes `D`, log bytes `L`, and one checkpoint body `M`, the interval cost is `(L + M) / D`. A larger batch reduces `L / D`, but does not eliminate `M`. Sparse snapshots remove empty entries, not the cost of rewriting all populated entries.

**Recommendation:** establish a checkpoint-write budget and a recovery-time target together. Increasing the interval trades write cost for replay; it is not a complete fix. If full snapshots cannot satisfy both targets, evaluate copy-on-write mapping pages or bounded checkpoint deltas with periodic full snapshots. Updating only dirty pages in the existing alternating bodies is not automatically safe: each body must still represent one complete checkpoint generation.

ADR-04's relocatable arenas solve growth and repeated append-space consumption by checkpoints. They do not solve checkpoint write volume, sorting cost, or pauses. Resolve those costs before treating arena relocation as the final metadata architecture. Recovery time also includes loading and validating the checkpoint, not just replaying its tail.

For batching, preserve ordering and flush semantics. Format version 1 forbids duplicate LBAs within one record, so collecting arbitrary queued writes requires an explicit rule for duplicates rather than blindly packing them together.

### Reclamation: the current engine has no sustained-overwrite regime

Even repeatedly overwriting one LBA eventually exhausts V0's finite log. Unlike an update-in-place data path, each overwrite consumes new physical space. Sequential appends alone do not establish a long-term performance advantage.

ADR-04 proposes pausing writes and copying all mapped blocks to a replacement image. That is a simple offline compaction baseline, not an online cleaner. It requires spare capacity for the replacement and a write pause proportional to the live data copied. It is appropriate only if the volume lifecycle permits those costs; it does not meet uninterrupted database service by itself.

**Recommendation:** keep replacement-image copying as a correctness reference or offline operation. For sustained writes, first compare segmented cleaning policies in a small trace model. A minimal candidate uses sealed segments, per-segment live-byte counts, and the authoritative LBA map to identify current records. Start with FIFO and greedy selection; add hot/cold placement only if it reduces measured copying enough to justify another write stream.

An online implementation would need more than a victim-selection policy:

- Treat a record as current only if the authoritative map still points to that exact record. A concurrent overwrite must not be replaced by a cleaner's stale copy.
- Persist relocated payloads and recoverable mapping state before reuse. A source remains protected while a checkpoint that recovery may select still references it; publishing only the newest root is insufficient under V2's fallback contract.
- Reserve space for relocation and metadata before admitting foreground writes that could exhaust it. Specify backpressure and exhaustion behavior.
- Give reused segments an identity that rejects stale records. Verify source checksums and recompute location-bound checksums when payloads move.

For a simplified steady-state cleaner, let `u` be the live fraction of the victim segments. Excluding headers and metadata:

```text
relocation writes / new payload writes = u / (1 - u)
total data writes / new payload writes = 1 / (1 - u)
```

Victims that are 50%, 80%, and 95% live imply 2×, 5×, and 20× write-only amplification. This is not overall device utilization. Original LFS's `2 / (1 - u)` cost also counts reading whole victim segments; do not compare that number directly with write-only amplification.

Discard is a separate state transition, not an optimization inferred from zero bytes. Filesystem deletion and database record deletion need not produce discard. A future discard implementation must define ordering, persistence, reads after discard, and prevention of old-data resurrection during replay or cleaning.

### Comparison: justify the extra mapping layer

FFS and ext4 normally overwrite already allocated file data without allocating another filesystem data block. Their metadata is persistent and cached, rather than requiring this engine's additional resident per-LBA map. They have different costs, including metadata updates, fragmentation, and, for ext4, journaling. The underlying SSD may still relocate every write internally.

The USENIX ’95 comparison found LFS about 50% faster than FFS without cleaning in its transaction workload. At 48% filesystem utilization, cleaning reduced LFS throughput by 34% relative to that no-cleaning result, eliminating the advantage. That is evidence against fresh-log-only benchmarks, not proof that modern log-structured systems lose. F2FS and LSVD provide counterexamples under their evaluated conditions.

**Recommendation:** require a concrete benefit from this layer: thin-volume isolation, useful placement, integrity checking, or a measured I/O improvement. Thin provisioning does not itself require log structuring. [Linux dm-thin](https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/thin-provisioning.html) is a relevant existing-functionality baseline, not another engine to implement.

Do not choose a shared cross-volume log solely from the volume count. It can improve batching and allocation but couples recovery, cleaning, and failure scope. Per-volume logs simplify whole-volume deletion and isolation. Measure those trade-offs before introducing a shared pool.

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
| What actually drives mapping memory? | Fix logical capacity; vary mapped occupancy and locality. Compare sequential population followed by uniform overwrites with clustered and moving-hotset writes. | Allocated bytes and peak RSS, bytes per mapped block, extent count, lookup cost, checkpoint sort memory, checkpoint load time. Separately measure idle per-device overhead. |
| Can checkpointing meet both budgets? | Count payload, footer, and checkpoint bytes independently. Vary logical size, batch size, and checkpoint interval on disposable files. | Amplification by category, pause duration, flush latency, checkpoint-load time, tail-replay time. Compare counts with the formula before interpreting throughput. |
| When does cleaning become expensive? | Use a metadata-only trace model with fixed physical capacity. Compare FIFO and greedy victims under uniform, hotset, and moving-hotset overwrites at several occupancy levels. Run beyond initial fill until metrics stabilize. | Copied-live bytes, victim live fractions, free-space reserve, and amplification over time. A placement model does not establish real I/O latency or crash safety. |
| Does the full stack benefit? | After correctness gates, compare the same filesystem/database settings with and without the engine. Include overwrite-only and deletion/reuse workloads; separate initial fill from active reclamation. | Throughput, p99 latency, actual discard traffic, bytes written at each observable boundary, RAM, and recovery correctness. Report backend topology, cache state, fsync policy, and integrity differences. |

Use only explicitly disposable backing files or approved disposable devices. A regular-file-backed ublk path includes a lower filesystem that a raw-device comparison does not; label that difference. Engine write counters are not SSD NAND-write counters. Until reclamation exists, report finite-log capacity and time to exhaustion, not sustained database throughput.

Start with mapping and checkpoint accounting. They can expose a failed assumption without adding a cleaner, changing the persistent format, or running a large database benchmark.
