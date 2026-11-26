//! ADR-11 read-only open: a view of one checkpoint, read without writing or claiming.
//!
//! A live writer may publish while the walk runs. Two format-4 invariants keep the walk exact:
//! records written after a root have greater LSNs, and blocks a durable root references stay
//! intact until that root's descriptor slot is overwritten. So a page that fails to decode is
//! either damage or proof that the selected slot changed; re-reading the slot tells them apart.
use super::{
    HeadId, RecordKind, VERSION,
    checkpoint_load::{self, Loaded},
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    log::Log,
    recovery::Walker,
    segment_table::SegmentState,
    small_records::{BackingGrowth, SegmentLink},
    tail::Probe,
};
use crate::{
    AlignedBlock, BLOCK_SIZE,
    accounted_io::BackingIo,
    backing::{self, BackingKind, OpenMode, Opened},
    invalid_data, read_block_exact,
};
use std::{fs::File, io, ops::RangeInclusive, path::Path};

/// Walks restarted because the writer replaced the selected root, before `open_readonly` stops.
const MAX_RESTARTS: u32 = 3;
/// Bytes 0..5 of a format-4 descriptor: magic and version.
const DESCRIPTOR_PREFIX: [u8; 5] = [b'V', b'B', b'L', b'C', VERSION];

/// One checkpoint of a format-4 backing, with per-segment accounting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeView {
    pub volume_id: u64,
    pub format_version: u8,
    pub backing: BackingInfo,
    pub logical_blocks: u32,
    pub provisioned_blocks: u64,
    pub blocks_per_segment: u32,
    /// Indexed by [`CheckpointSlot::index`].
    pub roots: [RootStatus; 2],
    pub selected: CheckpointSlot,
    /// Walks restarted because the writer replaced the selected root.
    pub restarts: u32,
    /// Indexed by segment number.
    pub segments: Box<[SegmentView]>,
    /// Nonzero mapping entries as of the selected checkpoint.
    pub mapped_blocks: u64,
    /// Records after the checkpoint, indexed by [`HeadId`] as `usize`.
    pub tail: [TailSummary; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackingInfo {
    pub kind: BackingKind,
    /// File length or device size.
    pub bytes: u64,
    /// `st_blocks` × 512; `None` for a device.
    pub host_allocated_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootStatus {
    Valid {
        checkpoint_lsn: u64,
    },
    /// The descriptor, or a page its walk read, failed validation.
    Invalid {
        reason: String,
    },
}

/// One segment as the selected root records it. Counts are 4 KiB blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentView {
    pub state: SegmentState,
    /// `Some` only while `Open`.
    pub head: Option<HeadId>,
    /// `Some` only while `Sealed` or `PendingFree`.
    pub seal_lsn: Option<u64>,
    /// The whole segment unless `Open`; an open segment up to its head boundary.
    pub used_blocks: u32,
    /// Payloads that the mapping selects.
    pub live_blocks: u32,
    /// Index, mapping and segment table pages that the root references.
    pub metadata_blocks: u32,
    /// Bytes of the segment that the host filesystem allocated; `None` for a device.
    pub host_allocated_bytes: Option<u64>,
}

/// Records on one head after the checkpoint, which the next live open replays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailSummary {
    pub records: u32,
    pub lsn_range: Option<RangeInclusive<u64>>,
}

impl TailSummary {
    pub(crate) fn add(&mut self, lsn: u64) {
        self.records += 1;
        let first = self.lsn_range.as_ref().map_or(lsn, |range| *range.start());
        self.lsn_range = Some(first..=lsn);
    }
}

/// Whole-log counts derived from the segments, in 4 KiB blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogTotals {
    pub used_blocks: u64,
    pub live_blocks: u64,
    pub metadata_blocks: u64,
    /// Used blocks that are neither live nor metadata.
    pub dead_blocks: u64,
    /// Free segments plus the unwritten blocks of open segments.
    pub free_blocks: u64,
    /// The unwritten blocks of open segments.
    pub open_free_blocks: u64,
    pub sealed_segments: u32,
    pub pending_free_segments: u32,
    pub open_segments: u32,
    pub free_segments: u32,
    /// The highest segment that is not free.
    pub high_water_segment: Option<u32>,
}

impl VolumeView {
    /// The checkpoint LSN of the selected root.
    #[must_use]
    pub fn checkpoint_lsn(&self) -> u64 {
        match self.roots[self.selected.index()] {
            RootStatus::Valid { checkpoint_lsn } => checkpoint_lsn,
            RootStatus::Invalid { .. } => unreachable!("the selected root is valid"),
        }
    }

    /// Byte offset of the first block of `segment`.
    #[must_use]
    pub fn segment_offset(&self, segment: u32) -> u64 {
        (u64::from(super::FIRST_SEGMENT_BLOCK)
            + u64::from(segment) * u64::from(self.blocks_per_segment))
            * BLOCK_SIZE as u64
    }

    /// The segment that `head` appends to.
    #[must_use]
    pub fn head_segment(&self, head: HeadId) -> Option<u32> {
        let segment = self
            .segments
            .iter()
            .position(|view| view.head == Some(head))?;
        u32::try_from(segment).ok()
    }

    #[must_use]
    pub fn totals(&self) -> LogTotals {
        let mut totals = LogTotals::default();
        let segment_blocks = u64::from(self.blocks_per_segment);
        for (index, segment) in self.segments.iter().enumerate() {
            let used = u64::from(segment.used_blocks);
            match segment.state {
                SegmentState::Free => {
                    totals.free_segments += 1;
                    totals.free_blocks += segment_blocks;
                    continue;
                }
                SegmentState::Open => {
                    totals.open_segments += 1;
                    totals.open_free_blocks += segment_blocks.saturating_sub(used);
                }
                SegmentState::Sealed => totals.sealed_segments += 1,
                SegmentState::PendingFree => totals.pending_free_segments += 1,
            }
            totals.used_blocks += used;
            totals.live_blocks += u64::from(segment.live_blocks);
            totals.metadata_blocks += u64::from(segment.metadata_blocks);
            totals.high_water_segment = u32::try_from(index).ok();
        }
        totals.free_blocks += totals.open_free_blocks;
        totals.dead_blocks = totals
            .used_blocks
            .saturating_sub(totals.live_blocks + totals.metadata_blocks);
        totals
    }
}

/// Reads one checkpoint of a format-4 backing. Never writes and claims nothing,
/// so a daemon may serve the same backing concurrently.
///
/// Returns the newest root whose descriptor and pages all validate, and the status of both
/// roots. Errors: `InvalidData` when no root validates; `InvalidInput` when the backing isn't a
/// format-4 backing; `ErrorKind::Other` when the writer replaced the root more than three times
/// during the walk; any I/O error.
pub fn open_readonly(backing_path: impl AsRef<Path>) -> io::Result<VolumeView> {
    open_readonly_with(backing_path.as_ref(), &mut || {})
}

/// `open_readonly` that calls `before_walk` before it walks each root, so a test can make the
/// writer publish at that exact point.
pub(crate) fn open_readonly_with(
    path: &Path,
    before_walk: &mut dyn FnMut(),
) -> io::Result<VolumeView> {
    let Opened { file, kind, .. } = backing::open(path, OpenMode::Inspect)?;
    let extent = backing::extent(&file, kind)?;
    if !extent.bytes.is_multiple_of(BLOCK_SIZE as u64) || extent.bytes < 2 * BLOCK_SIZE as u64 {
        return Err(not_format4(
            "the size is not a whole number of 4 KiB blocks",
        ));
    }
    let backing = BackingInfo {
        kind,
        bytes: extent.bytes,
        host_allocated_bytes: extent.host_allocated_bytes,
    };
    let mut reader = Reader {
        io: BackingIo::new(None)?,
        file,
        actual_blocks: extent.bytes / BLOCK_SIZE as u64,
        block: AlignedBlock([0; BLOCK_SIZE]),
    };
    let mut restarts = 0;
    // Bounded by MAX_RESTARTS: every iteration returns or counts one restart.
    loop {
        if let Some(walk) = reader.walk_newest(before_walk)? {
            return reader.view(walk, backing, restarts);
        }
        restarts += 1;
        if restarts > MAX_RESTARTS {
            return Err(io::Error::other(
                "the backing changed faster than the walk; try again",
            ));
        }
    }
}

fn not_format4(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("not a format-4 backing: {reason}"),
    )
}

/// What one descriptor slot holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Root {
    Valid(Box<CheckpointDescriptor>),
    Invalid(String),
    /// No format-4 magic: not a format-4 descriptor at all.
    Absent,
}

impl Root {
    fn status(&self) -> RootStatus {
        match self {
            Self::Valid(descriptor) => RootStatus::Valid {
                checkpoint_lsn: descriptor.checkpoint_lsn,
            },
            Self::Invalid(reason) => RootStatus::Invalid {
                reason: reason.clone(),
            },
            Self::Absent => RootStatus::Invalid {
                reason: "no format-4 descriptor".to_owned(),
            },
        }
    }

    fn lsn(&self) -> Option<u64> {
        match self {
            Self::Valid(descriptor) => Some(descriptor.checkpoint_lsn),
            Self::Invalid(_) | Self::Absent => None,
        }
    }
}

struct CompleteWalk {
    roots: [Root; 2],
    selected: CheckpointSlot,
    loaded: Loaded,
}

struct Reader {
    io: BackingIo,
    file: File,
    actual_blocks: u64,
    block: AlignedBlock,
}

impl Reader {
    /// Reads one descriptor, once more if it fails: the writer may be rewriting it.
    fn read_root(&mut self, slot: CheckpointSlot) -> io::Result<Root> {
        let mut last_error = String::new();
        for _attempt in 0..2 {
            read_block_exact(&mut self.io, &self.file, slot.block(), &mut self.block)?;
            match CheckpointDescriptor::decode(&self.block.0, slot) {
                Ok(descriptor) => return Ok(Root::Valid(Box::new(descriptor))),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    last_error = error.to_string();
                }
                Err(error) => return Err(error),
            }
        }
        if self.block.0[..DESCRIPTOR_PREFIX.len()] != DESCRIPTOR_PREFIX {
            return Ok(Root::Absent);
        }
        Ok(Root::Invalid(last_error))
    }

    fn read_roots(&mut self) -> io::Result<[Root; 2]> {
        let mut roots = [
            self.read_root(CheckpointSlot::Green)?,
            self.read_root(CheckpointSlot::Blue)?,
        ];
        if roots.iter().all(|root| *root == Root::Absent) {
            return Err(not_format4("neither descriptor has the format-4 magic"));
        }
        if let [Root::Valid(green), Root::Valid(blue)] = &roots
            && let Err(error) = green.validate_pair(blue)
        {
            // Startup refuses such a pair; here the newer root stays usable for the report.
            let older = if green.checkpoint_lsn >= blue.checkpoint_lsn {
                CheckpointSlot::Blue
            } else {
                CheckpointSlot::Green
            };
            roots[older.index()] = Root::Invalid(format!("{error} with the newer root"));
        }
        Ok(roots)
    }

    /// Walks the newest valid root, falling back to the other on damage. `None` means the
    /// selected slot changed during the walk, so the caller restarts.
    fn walk_newest(&mut self, before_walk: &mut dyn FnMut()) -> io::Result<Option<CompleteWalk>> {
        let mut roots = self.read_roots()?;
        let mut order = CheckpointSlot::BOTH;
        if roots[1].lsn() > roots[0].lsn() {
            order.swap(0, 1);
        }
        for slot in order {
            let Root::Valid(descriptor) = roots[slot.index()].clone() else {
                continue;
            };
            before_walk();
            let geometry = descriptor.geometry()?;
            let walked = checkpoint_load::load_candidate(
                &mut self.io,
                &self.file,
                &descriptor,
                self.actual_blocks,
                geometry.segment_count,
                &mut self.block,
            );
            match walked {
                Ok(loaded) => {
                    return Ok(Some(CompleteWalk {
                        roots,
                        selected: slot,
                        loaded,
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    if self.read_root(slot)? != Root::Valid(descriptor) {
                        return Ok(None);
                    }
                    roots[slot.index()] = Root::Invalid(error.to_string());
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "no valid format-4 checkpoint: green: {}; blue: {}",
                reason(&roots[0]),
                reason(&roots[1])
            ),
        ))
    }

    fn view(
        &mut self,
        walk: CompleteWalk,
        backing: BackingInfo,
        restarts: u32,
    ) -> io::Result<VolumeView> {
        let log = &walk.loaded.log;
        let geometry = log.geometry;
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(geometry.segment_count as usize)
            .map_err(|_| io::Error::other("segment view allocation failed"))?;
        for segment in 0..geometry.segment_count {
            segments.push(self.segment_view(log, backing.kind, segment)?);
        }
        Ok(VolumeView {
            volume_id: log.volume_id,
            format_version: VERSION,
            backing,
            logical_blocks: geometry.volume_blocks,
            provisioned_blocks: geometry.backing_blocks,
            blocks_per_segment: geometry.blocks_per_segment,
            roots: [walk.roots[0].status(), walk.roots[1].status()],
            selected: walk.selected,
            restarts,
            segments: segments.into_boxed_slice(),
            mapped_blocks: walk.loaded.live_blocks,
            tail: self.summarize_tail(log)?,
        })
    }

    fn segment_view(&self, log: &Log, kind: BackingKind, segment: u32) -> io::Result<SegmentView> {
        let entry = log.segments.entry(segment)?;
        let geometry = log.geometry;
        let start = u64::from(geometry.segment_start(segment)?) * BLOCK_SIZE as u64;
        let end = geometry.segment_end(segment)? * BLOCK_SIZE as u64;
        let host_allocated_bytes = match kind {
            BackingKind::RegularFile => Some(backing::allocated_bytes_in(&self.file, start..end)?),
            BackingKind::BlockDevice => None,
        };
        Ok(SegmentView {
            state: entry.state,
            head: entry.head,
            seal_lsn: entry.state.has_seal().then_some(entry.seal_lsn),
            used_blocks: log.segments.used_blocks(segment),
            live_blocks: log.segments.live_blocks(segment),
            metadata_blocks: log.segments.metadata_blocks(segment),
            host_allocated_bytes,
        })
    }

    /// Follows both head chains past the checkpoint with the replay rules. The summary ends at
    /// the first block that isn't the next record; on a live backing that can be a record in
    /// flight, which isn't damage.
    fn summarize_tail(&mut self, log: &Log) -> io::Result<[TailSummary; 2]> {
        let mut geometry = log.geometry;
        let mut walker = Walker::new(log.heads, log.next_lsn, u64::MAX);
        let mut tail = [TailSummary::default(), TailSummary::default()];
        // Bounded by the backing: every record occupies at least one block.
        for _ in 0..self.actual_blocks {
            let (id, probe) = match walker.next(&mut self.io, &self.file, geometry, log.volume_id) {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => break,
                Err(error) => return Err(error),
            };
            tail[id.index()].add(probe.header().lsn);
            let Probe::Metadata { header, block } = probe else {
                continue;
            };
            let followed = match header.kind {
                RecordKind::SegmentLink => SegmentLink::decode(&block.0, geometry, log.volume_id)
                    .and_then(|link| {
                        if link.head_id != id {
                            return Err(invalid_data("segment link names the other head"));
                        }
                        walker.follow_link(id, link.next_segment, geometry)
                    }),
                RecordKind::BackingGrowth => {
                    BackingGrowth::decode(&block.0, geometry, log.volume_id).and_then(|growth| {
                        if growth.new_backing_blocks > self.actual_blocks {
                            return Err(invalid_data("growth beyond the backing"));
                        }
                        geometry = geometry.with_backing(growth.new_backing_blocks)?;
                        Ok(())
                    })
                }
                _ => Ok(()),
            };
            if followed.is_err() {
                break;
            }
        }
        Ok(tail)
    }
}

fn reason(root: &Root) -> String {
    match root.status() {
        RootStatus::Valid { .. } => "valid".to_owned(),
        RootStatus::Invalid { reason } => reason,
    }
}
