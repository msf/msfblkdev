# ADR-09 Compression at relocation

Date: 2026-09-20 Author: Miguel Filipe Status: deferred draft, split from ADR-08. Codec, encoding
and activation remain undecided. Nothing is implemented. On-disk format: future extension; version
or incompatible-feature gate must be decided before activation. Compressed records are not valid
format-4 records. Would extend: [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md) Related:
[workbench](TMD-WORKBENCH.md), [original technical review](docs/REVIEW-ADR-08.md)

## Context

ADR-08 reclaims obsolete records by relocating live raw data. It must work without compression.
Compression may reduce physical occupancy and relocation writes, but adds CPU work, read
amplification and shared-chunk accounting. These mechanisms need separate correctness and
performance evidence.

The target includes fast NVMe backing. A codec can save space while slowing reads. Relocated data
may have a long overwrite lifetime and still be read frequently. Neither age nor cleaner selection
establishes that decompression is cheap for the workload.

## Decision

Deliver raw reclamation first. Evaluate compression at relocation as a separate extension with an
explicit off mode and a complete raw-cleaning baseline. Enabling compression must not be necessary
for correctness, capacity reservation or progress.

An off mode emits ordinary `Data` records through the baseline cleaner. It does not wrap raw bytes
in a compressed-record container. No codec trait or no-op implementation is required before a real
compressor is introduced.

Once compressed records exist, turning compression off stops new compressed output. Readers must
still decode existing records. With compression off, the cleaner must be able to emit raw survivors
from those records. This requires sufficient reserved output space.

The existing two fixed heads and writer ownership stay unchanged. New payloads use User. Accepted
relocation copies use Relocation. Raw and compressed records may share relocation segments once this
extension is supported.

Format-4 readers reject reserved kind 6 and mapping bit 31. Before assigning them compressed
semantics, choose a new format version or an incompatible-feature gate. That choice must specify
upgrades and rejection by old readers.

## What must we measure?

Report byte counters and ratios over intervals and for the complete run. Calculate ratios from sums
of bytes, not averages of per-chunk ratios. Report a ratio as unavailable when its denominator is
zero.

Label each result with the codec, level, chunk size, workload, backing and CPU. Separate attempts,
accepted output and current occupancy. Repeated relocation must not inflate apparent live-space
savings.

| Measurement | Required definition |
|---|---|
| Codec ratio | `input_bytes / encoded_bytes` for completed compression attempts, before storage framing or padding. Count rejected-for-poor-saving attempts too. Report accepted-output totals separately. |
| Effective storage saving | Compare the bytes of equivalent raw records with actual emitted records for the same accepted payload set. Include codec framing, checksums, footers, block padding and raw fallback. Report absolute bytes saved and `1 - actual_bytes / raw_equivalent_bytes`. |
| Current occupancy | Current logical payload bytes, retained physical raw/compressed record bytes, and bytes protected by roots or views. Count each shared chunk/footer once; dead payloads inside a retained chunk do not make its bytes free. Report allocated backing separately. |
| Routing and wasted work | Attempted, compressed, raw-fallback and stale-rejected chunks/payload bytes, with reasons. Report size/savings distributions so a mean does not hide incompressible input. |
| CPU and reads | Compression/decompression time and bytes, decoder calls, requested logical read bytes and actual backing bytes read, including footer reads and block rounding. Report read amplification, CPU per useful byte, throughput and P50/P99/P99.9 read latency. |
| Cleaning cost | User, relocation and metadata bytes written; victim bytes read; logical live bytes moved; net segment capacity freed; raw-to-compressed, compressed-to-raw and recompressed bytes. Include workspace/proposal memory peaks. |

For example, 64 KiB of input encoded into 30 KiB gives a codec ratio of about 2.13×. An isolated
record with 32 KiB of rounded data and a 4 KiB footer occupies 36 KiB.

The equivalent raw record occupies 68 KiB including its footer. The effective saving is about 47.1%,
before other metadata or segment-tail costs. Real reports must use actual packing. Do not charge a
shared footer to every chunk.

Keep the raw-cleaner counters from ADR-08 available with compression disabled. When no compression
was attempted, codec-specific ratios are unavailable. Do not report a fabricated 1:1 measurement.

Compare total engine writes and actual backing allocation as well as the codec's output size. Define
when counters change at attempt, acceptance and publication boundaries. Runtime occupancy and
recovery reconstruction must agree.

## Which codec and chunk size?

Compare raw output, LZ4, Zstd fast modes and Zstd level 1 on captured database pages. LZ4 is the
leading candidate for evaluation because foreground decode cost matters. This is a preference to
test, not an accepted codec decision. Zstd fast modes use the Zstd format; they are not another
codec.

Start with independent 4, 16 and 64 KiB chunks. Expand the sweep only where results justify it. For
each candidate:

1. Measure codec CPU and ratio in isolation.
2. Run the engine with the same aged read/update trace.
3. Record achieved device bandwidth and concurrency.

Include sequential reads, random 4 KiB reads, read-heavy immutable data and incompressible input.
Advertised sequential NVMe bandwidth is not a random-read latency measurement.

The inherited starting point was Zstd level 1, sixteen 4 KiB payloads per frame, and raw fallback
below 12.5% savings. These are unmeasured candidates, not requirements.

Choose compressed or raw output using actual storage savings, including block rounding and record
overhead. Codec choice, chunk size and the threshold must pass the read-latency and cleaner-progress
gates together.

The footer already provides boundaries, output counts and checksums. Independent LZ4 blocks or
ordinary Zstd frames can use that container. The draft does not require Zstd Seekable Format. Keep
dependencies minimal, disable default features and select only required features after choosing an
implementation.

EBS §2.2 reports mean LZ4 ratio 50.1% in its workload
([paper](https://www.usenix.org/system/files/fast24-zhang-weidong.pdf)). That is prior art for
compression during garbage collection, not a prediction for this engine. Define our ratio direction
explicitly rather than reusing an ambiguous percentage.

## What must the extension specify?

The raw allocator, checkpoint publication, recovery and freeing barrier remain owned by ADR-08. The
extension inherits its `checkpoint_lsn`, `seal_lsn` and packed segment flags without another
generation counter. Their layouts are defined in [ADR-08's encoding
reference](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#encoding-reference). This extension must
specify their interaction with compressed records before activation:

| Mechanism | Required contract |
|---|---|
| Record and mapping encoding | Codec ID, chunk boundaries, decoded sizes, LBA ordering, checksum inputs and old-reader rejection. A lookup's payload index identifies both its chunk and its offset within the decoded chunk. |
| Read and replay validation | Verify the footer before following its offsets; validate bounds, counts, uniqueness and non-overlap. Verify encoded bytes before bounded decoding. Require the exact decoded size and verify the requested payload against the current mapping and footer. Replay must install only accepted payloads. |
| Conditional apply | If some payloads become stale during compression, choose a safe persisted representation. Rejecting the entire affected chunk is the simplest candidate. Keeping rejected payloads in a footer that replay installs is unsafe. |
| Checksums | Retain volume/LBA-bound decoded-payload checksums and location-bound encoded-chunk checksums. Disable optional codec content checksums only where these checks cover the required integrity contract; retain any mandatory framing checks. |
| Accounting and reserve | Deduplicate shared chunk/footer occupancy. Bound workspace, raw fallback and off-mode expansion. Progress must not depend on a guessed compression ratio or savings from rejected proposals. |
| Recleaning and discard | Decode and verify live chunks, drop dead payloads and rebuild survivors. Whole unchanged chunks may be copied if every member passes current-source validation. Recompute destination-bound checksums. A discard can make a payload dead without freeing its shared chunk. |

The earlier review's T3, T10, T11, the chunk-capacity part of T12 and compression-specific parts of
T14–T16 remain open here. The generic source-reuse, reservation and metadata-publication findings
still belong to ADR-08. Moving a finding does not resolve it.

## Is a decoded-chunk cache needed?

A small decoded-chunk read cache is deferred beyond initial compression acceptance. With a fixed
byte budget, it could retain decompressed chunks and their verified footer metadata. Different
payloads in the same chunk could then reuse the bytes already read and decoded.

The filesystem page cache above ublk receives only the requested bytes. It does not retain the rest
of a chunk decoded by this engine.

Start evaluation with one or a few entries. Measure hits, avoided I/O and decompression, and read
latency. Reads must still resolve the current mapping and verify the requested payload checksum.

Cache identity or invalidation must prevent stale hits after segment reuse. A physical address alone
is insufficient.

Measure compression first with this cache disabled. If a cache is later justified, report hits,
misses, evictions, bytes retained and avoided reads/decodes. Use the same workload and memory
budget.

## Acceptance and delivery

Each delivery has its own evidence requirements. No-compression acceptance remains independently
required.

### 1. Measure against raw cleaning

Publish the byte counters, ratio definitions and captured-page codec/chunk comparison. Verify
counter arithmetic with raw fallback and stale-rejected output. This work may run independently of
ADR-08 implementation.

### 2. Review the extension format

Close the rules for encoding, compatibility, partial acceptance, checksums, decoder bounds,
occupancy and reserve. Before emitting compressed records in ordinary volumes, test malformed
records and crash boundaries in isolated fixtures.

### 3. Integrate optional relocation compression

Test mixed records, recompression, discards, stale proposals and disabling compression after data
exists. Recovery must preserve the baseline's durability guarantees. Runtime occupancy and rebuilt
occupancy must agree.

### 4. Gate activation on aged workloads

Fix acceptance thresholds before comparing results. No threshold is claimed here.

Compare enabled and disabled runs using the same traces and backing capacity. Report effective
savings, amplification, CPU, memory, throughput and latency.

## Inherited encoding sketch, not an accepted format

This retains the former ADR-08 footer proposal for review, with chunk fields now stored in parallel
arrays. It is Zstd-specific and does not select Zstd for the extension. A future header
version/feature gate, codec IDs and decoder limits are still required.

Each chunk needs 16 bytes: two `u32` fields and one `u64` checksum. Parallel arrays do not reduce
that size. The earlier sketch left 80 bytes unused. Using those bytes raises capacity from 19 to 24
chunks. The payload limit remains 304. Enforce `payload_count <= min(304, 24 * payloads_per_chunk)`.
The 4 KiB chunk experiment would require extending the old permitted chunk-size range. Do not
implement this sketch verbatim.

```rust
/// A CompressedData record is `chunk_count` zstd frames concatenated and padded
/// to whole blocks, followed by this footer. Chunk `c` holds payloads
/// `c * payloads_per_chunk .. min(payload_count, (c + 1) * payloads_per_chunk)` of the LBA array.
/// LBAs are sorted; the final chunk can be partial.
pub struct CompressedDataFooter {
    pub header: RecordHeader,       // 0..32, kind = CompressedData
    pub payload_count: u16,         // 32: 1 to 304
    pub chunk_count: u16,           // 34: 1 to 24; ceil(payload_count / payloads_per_chunk)
    pub payloads_per_chunk: u8,         // 36: payloads per chunk, 4 to 32; 16 initially
    pub compression: CompressionAlgorithm, // 37
    // 38..40 zero
    pub payload_lbas: [u32; 304],   // 40..1256: sorted
    pub payload_checksums: [u64; 304], // 1256..3688: XXH3-64 of the uncompressed payload, seeded by
                                    //   volume_id, over LE(lba) || payload. Location-free, so the
                                    //   cleaner computes it once and the writer copies it.
    pub frame_offsets: [u32; 24],   // 3688..3784: byte offsets from the record's first block
    pub frame_lengths: [u32; 24],   // 3784..3880: encoded frame lengths in bytes
    pub frame_checksums: [u64; 24], // 3880..4072: destination-bound checksums
    // 4072..4096 metadata trailer
}

#[repr(u8)]
pub enum CompressionAlgorithm {
    Zstd = 1,
}
```

Chunk `c` uses entry `c` in all three frame arrays. Entries at or beyond `chunk_count` are zero.
Each frame checksum uses XXH3-64 seeded by `volume_id`, over `LE(footer block, chunk number) ||
frame`. The writer computes it at apply, when it knows the destination footer block.
