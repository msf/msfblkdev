# ADR-11 blkctl: volume inspection and offline check

Date: 2026-11-25. Revised: 2026-11-26.
Author: Miguel Filipe. Authored by LLM, reviewed by human.
Status: proposed; not implemented
Updates: [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md) (two existing invariants become requirements for live readers)
Related: [ADR-05 structural checker](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md#independent-logical-and-structural-checks)

## Context

A recurring review question is: "Why not store each segment as a file on a filesystem? Operations
and observability would be easier, and we'd get thin provisioning for free."

Today no tool answers it. The only usage report is `Volume::space_usage()`, which runs inside the
process that owns the volume. Nothing inspects a backing that is closed, or a live backing (one
that a daemon is serving).

A filesystem answers these questions with `df`, `du`, `btrfs filesystem usage` and `fsck`. We want
the same for our backing, whether it is a regular file or a raw block device: the logical size, the
physical use, segment states, live and dead space per segment, identities, and a full integrity
check.

This ADR doesn't revisit ADR-01's choice of a raw log over a filesystem. It removes the
observability argument for segment files.

## Decision

We build:

- `open_readonly()` in the engine: reads one checkpoint into a view. It never writes and claims nothing.
- `open_live()` in the engine: today's `open` family, renamed. It takes ownership of the backing, recovers and serves requests.
- `blkctl info`: validates the key metadata and prints the view. It is fast and safe on a live backing.
- `blkctl check`: verifies every checksum and reference in every non-free segment. It targets an offline backing.

`blkctl` is a new binary in the `rust` crate. It parses arguments by hand and adds no dependency.
Both commands accept a regular file or a block device. Nothing here changes the on-disk format.

```text
blkctl info  [--segments] [--blocks] [-v] [--color=auto|always|never] <backing>
blkctl check [-v] [--color=auto|always|never] <backing>
```

A root is one of the two checkpoint descriptors, green or blue (ADR-08).

### What do used, live, metadata, dead and free mean?

We count 4 KiB blocks in the log, as ADR-08's T10 and `SpaceUsage` do. ADR-08's "live record" also
covers current metadata; here metadata has its own count.

| Term | Meaning | Source |
|---|---|---|
| used | Blocks written in a non-free segment: a whole sealed or pending-free segment, or an open segment up to its head boundary. | segment table, descriptor |
| live | Payload blocks that the mapping selects. Each mapped logical block address (LBA) has exactly one live block. | mapping pages |
| metadata | Index, mapping and segment table pages that the root references. The two descriptors sit outside the log. | root walk |
| dead | used - live - metadata: overwritten or discarded payloads, superseded metadata pages, data-record footers and segment links. | derived |
| free | Free segments plus the unwritten blocks of open segments. A pending-free segment holds nothing live but isn't free until the second publication (ADR-08). | derived |
| live fraction | A segment's live blocks divided by its size: ADR-08's utilization `u`. | derived |

Each count exists per segment and for the whole log. The format doesn't store live counts. `info`
derives them from the root's mapping pages, as startup does: each nonzero mapping entry adds one
live block to the segment that holds it.

### How fast must `info` be?

`info` must finish in under 5 seconds on a fully mapped 500 GiB volume on local NVMe.

That volume has 390,096 mapping pages of 336 entries, so `info` reads about 1.5 GiB in 4 KiB
blocks: about 78,000 reads per second (calculated, not measured). One outstanding read at a time
can't reach that rate, so the walk runs in batches of `N` pages:

1. Take the next `N` page IDs and look up their physical blocks in the mapping index.
2. Sort the batch by physical block and submit all `N` reads through `io_uring`.
3. Wait for the batch, then decode and validate each page and add its entries to the counts.

A volume with unmapped pages reads less, because a zero index pointer has no page.

### What does `open_readonly()` return?

```rust
/// Reads one checkpoint of a format-4 backing. Never writes and claims nothing,
/// so a daemon may serve the same backing concurrently.
pub fn open_readonly(backing_path: impl AsRef<Path>) -> io::Result<VolumeView>;

pub struct VolumeView {
    pub volume_id: u64,
    pub format_version: u8,
    pub backing: BackingInfo,
    pub logical_blocks: u32,
    pub provisioned_blocks: u64,
    pub blocks_per_segment: u32,
    pub roots: [RootStatus; 2],        // indexed by CheckpointSlot: green, blue
    pub selected: CheckpointSlot,
    pub restarts: u32,                 // walks restarted because the selected root moved
    pub segments: Box<[SegmentView]>,  // indexed by segment number
    pub mapped_blocks: u64,
    pub tail: [TailSummary; 2],        // indexed by HeadId: user, relocation
}

pub enum BackingKind { RegularFile, BlockDevice }

pub struct BackingInfo {
    pub kind: BackingKind,
    pub bytes: u64,                          // file length or device size
    pub host_allocated_bytes: Option<u64>,   // st_blocks × 512; None for a device
}

pub enum RootStatus {
    Valid { checkpoint_lsn: u64 },
    Invalid { reason: String },        // descriptor or walked page that failed validation
}

pub struct SegmentView {
    pub state: SegmentState,                 // Free, Open, Sealed, PendingFree
    pub head: Option<HeadId>,                // Some only while Open
    pub seal_lsn: Option<u64>,               // Some only while Sealed or PendingFree
    pub used_blocks: u32,                    // whole segment unless Open
    pub live_blocks: u32,
    pub metadata_blocks: u32,
    pub host_allocated_bytes: Option<u64>,   // SEEK_DATA/SEEK_HOLE; None for a device
}

pub struct TailSummary {
    pub records: u32,
    pub lsn_range: Option<RangeInclusive<u64>>,
}
```

The engine's `SegmentState`, `HeadId` and `CheckpointSlot` become public. `open_readonly()`
decodes each mapping page, adds its entries to the segment counters and then drops it.

### How does `info` stay consistent on a live backing?

We can't read all metadata atomically while the writer runs. We don't need to, because format 4
already guarantees two invariants:

1. Every record written after a root is published has a logical sequence number (LSN) greater than
   that root's checkpoint LSN. The global LSN sequence is monotonic (ADR-08
   [Segments and record chains](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#segments-and-record-chains)).
2. The writer doesn't overwrite a block that a durable root references until that root's
   descriptor slot is overwritten. This is the two-publication barrier (ADR-08
   [When may the writer reuse a victim?](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#when-may-the-writer-reuse-a-victim)).

Each page decode checks kind, page ID, `boundary == pointer`, volume ID, checksums and
`lsn <= checkpoint_lsn`. By invariant 1, a reused block fails that check. So every page that
decodes is the page the root references, and a walk where every decode succeeds is exact as of the
root's checkpoint LSN, even if the writer published in the meantime.

`info` reads this way:

1. Read both descriptors with `O_DIRECT`. If a descriptor fails validation, read it once more, because the writer may be rewriting it.
2. Select the valid root with the highest checkpoint LSN.
3. Walk its index, mapping and segment table pages.
4. On a decode failure, read both descriptors again. If the selected slot changed, restart from the newest root. If it didn't change, invariant 2 says the root is damaged: report it and fall back to the other root, as startup does.
5. Stop after 3 restarts and report that the backing changed faster than the walk.
6. Walk each head's tail from the root's boundary through `SegmentLink` records, with the replay chaining rules. Stop at the first block that isn't the next record. On a live backing that block can be a record in flight, which isn't an error.

`info` claims nothing, so `open_live()`'s ownership doesn't block it.

### What does `blkctl info` print?

`info` validates what it reads: both descriptors (checksums, identity, geometry and agreement
between the pair) and every index, mapping and segment table page of the selected root. It doesn't
read payloads.

Illustrative output for a 128 MiB volume on 185 MiB of regular-file backing:

```text
$ blkctl info disk.img
volume     3f9a1c075e2b40d1   format 4   block 4 KiB
backing    disk.img   regular file

size
  logical             128.0 MiB
  mapped               96.4 MiB    75.3% of logical
  provisioned         185.0 MiB
  device              185.0 MiB
  host allocated      131.3 MiB

log        23 segments × 8.0 MiB = 184.0 MiB
  used                131.2 MiB    15 sealed or pending-free, 2 open
  high-water mark     160.0 MiB    end of segment 19
  live                 96.4 MiB    73.5% of used
  metadata              0.6 MiB
  dead                 34.2 MiB    26.1% of used
  free                 52.8 MiB    6 segments + 4.8 MiB in open segments

segments   sealed 14   pending-free 1   open 2   free 6
  user head           segment 7
  relocation head     segment 12
  oldest seal LSN     1032

checkpoints
  green               LSN 48213   valid   selected
  blue                LSN 48190   valid
  tail                user 37 records (LSN 48214..48251), relocation 0

live fraction of sealed and pending-free segments
    0-10%  ██            1
   10-20%                0
   20-30%  ██            1
   30-40%                0
   40-50%  ██            1
   50-60%  ██            1
   60-70%  ████          2
   70-80%  ████          2
   80-90%  ██████        3
  90-100%  ████████      4
```

| Field | Definition | Source |
|---|---|---|
| logical | `volume_blocks` × 4 KiB | descriptor |
| mapped | nonzero mapping entries × 4 KiB, as of the checkpoint | mapping pages |
| provisioned | `backing_blocks` × 4 KiB: the bound the engine may use | descriptor |
| device | file length or device size | `seek(End)` |
| host allocated | `st_blocks` × 512. Regular files only. | `fstat` |
| high-water mark | byte offset of the end of the highest non-free segment: the smallest backing that holds the log without compaction | segment table |
| oldest seal LSN | lowest seal LSN of sealed and pending-free segments | segment table |
| tail | records after the checkpoint on each head chain | tail walk |

Used, live, metadata, dead and free follow
[What do used, live, metadata, dead and free mean?](#what-do-used-live-metadata-dead-and-free-mean).

`--segments` adds one row per segment, like `du` over segment files:

```text
  SEG  STATE              SEAL_LSN    USED   LIVE   META   DEAD   HOST
    0  sealed                 1032     8.0    7.9    0.1    0.0    8.0
    1  sealed                 2210     8.0    1.2    0.0    6.8    8.0
    7  open user                 -     4.0    3.6    0.2    0.2    4.0
   12  open relocation           -     7.2    7.2    0.0    0.0    7.2
   20  free                      -       0      -      -      -      0
```

### What does `blkctl check` verify?

`check` is the structural checker that ADR-05 asks for. It shares the record codecs and
`Geometry` with the engine. It doesn't call recovery, `Segments`, `MappingIndex` or the cleaner,
so it doesn't trust the code it checks. It never writes. It verifies:

1. Roots: both descriptors decode, and a valid pair agrees on identity and geometry.
2. Metadata, for each valid root: every index, mapping and segment table page decodes, and each lies in a segment that the root's table marks non-free.
3. Segments, for each non-free segment of the selected root: the record chain from the segment start has valid framing and checksums, strictly increasing LSNs and matching `previous_boundary` links. A sealed or pending-free segment ends with a `SegmentLink` in its link slot whose LSN equals the seal LSN.
4. Payloads: every payload checksum matches its data-record footer. A mismatch on a live payload is an error. A mismatch on a dead payload is a warning: reads can't return it, but it shows damage on the medium.
5. References: every nonzero mapping entry points at a payload whose footer names that LBA with the entry's checksum. No physical block is mapped twice. The mapped count equals the live count from the segment walk.
6. Tail: the tail records of both heads validate completely, including payload checksums.
7. Accounting: in every segment, live + metadata ≤ used.

`check` targets an offline backing and refuses one that `open_live()` owns. Running it on a live
backing is a COULD: if we add it, `check` must not report a race with the writer as damage.

### How do we format output?

- Integers print in decimal: LSNs, segment numbers, block numbers, record and LBA counts.
- The volume ID and checksums print in hexadecimal, 16 digits. Checksums appear only in error lines.
- Sizes use binary units with one decimal. `--blocks` prints 4 KiB block counts instead, in the summary and in the `--segments` table.
- The summary fits in 80 columns.
- Color uses ANSI escape codes. `--color=auto`, the default, enables it only when stdout is a TTY, `NO_COLOR` is unset and `TERM` isn't `dumb`.
- An error line names the segment, block, LSN and LBA where they apply. Both commands print the first `MAX_ERRORS_SHOWN` errors and a total; `-v` prints all of them.

```text
error  segment 9 block 73810 LSN 30117 LBA 1204: payload checksum 5a0c3e91d2f7a640, footer 9c1e44a07b2d1f03
```

| Exit code | `info` | `check` |
|---|---|---|
| 0 | Every descriptor and page it read is valid. | No errors. No tail. |
| 1 | Not used. | No errors. A tail exists that the next `open_live()` replays. |
| 4 | A descriptor or page failed validation. The view still prints if one root is valid. | Errors found. |
| 8 | Could not run: I/O error, not a format-4 backing, or the backing changed faster than the walk. | Could not run: I/O error, not a format-4 backing, or `open_live()` owns the backing. |

## Consequences

- `info` reports the state as of the checkpoint and only summarizes the tail. Replaying the tail would make `info` a second recovery implementation. On a busy live backing, the counts lag by up to one checkpoint interval.
- `info` doesn't report a segment's first LSN. Format 4 doesn't store it, and finding it means reading every record in the segment.
- Dead space includes data-record footers and segment links. Separating them from dead payloads means reading the segments.
- `info`'s safety on a live backing depends on invariants 1 and 2. A format change that weakens either must update this ADR.

Tests we propose:

- `info` and `check` leave the image byte-identical.
- After a clean close, `info` agrees with `Volume::space_usage()`.
- `info` runs repeatedly against a writer that overwrites and forces checkpoints. It never reports damage, and at least one run records a restart.
- `check` detects one injected fault in each structure: descriptor, index page, mapping page, table page, live payload, data footer and segment link.
- `info` finishes in under 5 seconds on a fully mapped 500 GiB volume on local NVMe.

## Open questions

- A 500 GiB volume with 32 MiB segments has 16,000 segments, which makes a long `--segments` table. A compact map with one character per segment may serve better.

## Next steps

1. Rename the `open` family to `open_live` and add `open_readonly()` with the walk above.
2. Add `blkctl info` and `--segments`, with the tests above.
3. Add `blkctl check`.

## Appendix: FAQ

### Why not change the on-disk format to persist per-segment live counts in the checkpoint?

The mapping pages are the only source of truth for what is live, and startup already derives the
counts from them. Persisted counts would be a second copy of that truth. Every publication would
have to keep the copy equal to the pages it describes, and nothing in the format would enforce that
equality, so a bug or a race could make them disagree silently. A reader couldn't trust the copy
without walking the pages anyway.
