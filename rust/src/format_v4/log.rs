//! The writer's log state: both heads, the segment table, the resident index and appends.
use super::{
    Geometry, HeadId, RecordHeader, RecordKind,
    descriptor::CheckpointSlot,
    heads::{Head, LogPosition},
    mapping_index::MappingIndex,
    segments::Segments,
    small_records::SegmentLink,
    storage::{self, BATCH_BLOCKS},
};
use crate::{AlignedBlock, BLOCK_SIZE, accounted_io::BackingIo, invalid_data};
use std::{fs::File, io};

/// Bytes appended per head and record class, for amplification reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogWrites {
    pub user_payload_blocks: u64,
    pub user_footer_blocks: u64,
    pub relocation_payload_blocks: u64,
    pub relocation_footer_blocks: u64,
    pub metadata_blocks: u64,
    pub link_blocks: u64,
    pub link_fsyncs: u64,
}

pub(crate) struct Log {
    pub(crate) volume_id: u64,
    pub(crate) geometry: Geometry,
    pub(crate) heads: [Head; 2],
    pub(crate) segments: Segments,
    pub(crate) index: MappingIndex,
    /// The LSN the next record receives.
    pub(crate) next_lsn: u64,
    pub(crate) active_slot: CheckpointSlot,
    /// Checkpoint LSN recorded by each valid durable root, by slot.
    pub(crate) root_lsn: [Option<u64>; 2],
    /// Blocks appended on both heads since the active root was published.
    pub(crate) blocks_since_checkpoint: u64,
    pub(crate) writes: LogWrites,
    /// `BATCH_BLOCKS` aligned blocks for staging metadata records.
    pub(crate) scratch: Box<[AlignedBlock]>,
}

impl Log {
    pub(crate) fn head(&self, id: HeadId) -> Head {
        self.heads[id.index()]
    }

    pub(crate) fn head_mut(&mut self, id: HeadId) -> &mut Head {
        &mut self.heads[id.index()]
    }

    pub(crate) fn checkpoint_lsn(&self) -> u64 {
        self.root_lsn[self.active_slot as usize].expect("active root is valid")
    }

    /// Records with an LSN at or below this value are off both roots' replay paths.
    pub(crate) fn protected_lsn(&self) -> u64 {
        self.root_lsn
            .iter()
            .flatten()
            .copied()
            .min()
            .expect("at least one valid root")
    }

    pub(crate) fn position(&self, head: HeadId) -> LogPosition {
        self.head(head).position()
    }

    fn take_lsn(&mut self) -> io::Result<u64> {
        let lsn = self.next_lsn;
        self.next_lsn = lsn
            .checked_add(1)
            .ok_or_else(|| invalid_data("record LSN exhausted"))?;
        Ok(lsn)
    }

    /// Closes the head's segment with a durable link and opens the lowest free segment.
    /// The link is fsynced before anything is written to the new segment (freshness rule).
    pub(crate) fn advance_head(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        id: HeadId,
    ) -> io::Result<()> {
        let next_segment = self.segments.allocate(id)?;
        let head = self.head(id);
        let lsn = self.take_lsn()?;
        let header = RecordHeader {
            kind: RecordKind::SegmentLink,
            volume_id: self.volume_id,
            lsn,
            previous_boundary: head.last_boundary,
            boundary: head.link_block(self.geometry),
        };
        let link = SegmentLink {
            header,
            next_segment,
            head_id: id,
        };
        link.encode(self.geometry, &mut self.scratch[0].0)?;
        storage::write_blocks(io, backing, header.boundary, &self.scratch[..1])?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("segment-link-write-complete-{}", id.name()))?;
        storage::sync(io, backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("segment-link-fsync-complete-{}", id.name()))?;
        self.segments.record_used(head.segment, 1);
        self.segments.seal(head.segment, id, lsn)?;
        let geometry = self.geometry;
        let head = self.head_mut(id);
        head.advance(header);
        head.open(next_segment, geometry)?;
        self.writes.link_blocks += 1;
        self.writes.link_fsyncs += 1;
        self.blocks_since_checkpoint += 1;
        Ok(())
    }

    /// Makes room for `blocks` one-block or payload blocks on the head, advancing it if needed.
    pub(crate) fn ensure_room(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        id: HeadId,
        blocks: u32,
    ) -> io::Result<()> {
        assert!(
            blocks < self.geometry.blocks_per_segment,
            "a fresh segment must hold the request"
        );
        if self.head(id).room(self.geometry) < blocks {
            self.advance_head(io, backing, id)?;
        }
        Ok(())
    }

    /// The header of the next one-block metadata record on `id`. The caller must have ensured
    /// room; the LSN is consumed by `commit_metadata`.
    pub(crate) fn metadata_header(&self, id: HeadId, kind: RecordKind) -> RecordHeader {
        assert!(!kind.is_data(), "data records carry payloads");
        self.head(id).header(kind, self.volume_id, self.next_lsn, 0)
    }

    /// Writes `blocks` consecutive one-block metadata records whose headers were produced in
    /// order by `metadata_header` with successive LSNs.
    pub(crate) fn commit_metadata_batch(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        id: HeadId,
        headers: &[RecordHeader],
    ) -> io::Result<()> {
        assert!(!headers.is_empty() && headers.len() <= BATCH_BLOCKS);
        let first = headers[0];
        assert_eq!(
            first.boundary,
            self.head(id).next_block,
            "batch starts at the head"
        );
        assert_eq!(
            first.lsn, self.next_lsn,
            "batch consumes the next LSNs in order"
        );
        storage::write_blocks(io, backing, first.boundary, &self.scratch[..headers.len()])?;
        for header in headers {
            let lsn = self.take_lsn()?;
            assert_eq!(lsn, header.lsn, "batch LSNs are consecutive");
            self.head_mut(id).advance(*header);
        }
        let segment = self.head(id).segment;
        self.segments.record_used(segment, headers.len() as u32);
        self.writes.metadata_blocks += headers.len() as u64;
        self.blocks_since_checkpoint += headers.len() as u64;
        Ok(())
    }

    /// Ensures room and writes one metadata record encoded by `encode` into scratch block 0.
    pub(crate) fn append_metadata(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        id: HeadId,
        kind: RecordKind,
        encode: impl FnOnce(&Self, RecordHeader, &mut [u8; BLOCK_SIZE]) -> io::Result<()>,
    ) -> io::Result<RecordHeader> {
        self.ensure_room(io, backing, id, 1)?;
        let header = self.metadata_header(id, kind);
        let mut block = AlignedBlock([0; BLOCK_SIZE]);
        encode(self, header, &mut block.0)?;
        self.scratch[0] = block;
        self.commit_metadata_batch(io, backing, id, &[header])?;
        Ok(header)
    }

    /// The header of a data record with `count` payloads on `id`. The caller ensured room.
    pub(crate) fn data_header(&self, id: HeadId, count: usize) -> RecordHeader {
        let head = self.head(id);
        assert!(count >= 1 && count <= head.payload_room(self.geometry));
        head.header(
            RecordKind::Data,
            self.volume_id,
            self.next_lsn,
            count as u32,
        )
    }

    /// Accounts a completed data record write on `id`.
    pub(crate) fn data_committed(&mut self, id: HeadId, header: RecordHeader) -> io::Result<()> {
        let count = header.payload_count(self.geometry)? as u64;
        let lsn = self.take_lsn()?;
        assert_eq!(lsn, header.lsn);
        self.head_mut(id).advance(header);
        let segment = self.head(id).segment;
        self.segments.record_used(segment, count as u32 + 1);
        match id {
            HeadId::User => {
                self.writes.user_payload_blocks += count;
                self.writes.user_footer_blocks += 1;
            }
            HeadId::Relocation => {
                self.writes.relocation_payload_blocks += count;
                self.writes.relocation_footer_blocks += 1;
            }
        }
        self.blocks_since_checkpoint += count + 1;
        Ok(())
    }

    /// Replaces a mapping page's current snapshot location and keeps segment references exact.
    pub(crate) fn snapshot_written(&mut self, page: u32, block: u32) -> io::Result<()> {
        let old = self.index.set_snapshot_block(page, block);
        self.segments.move_metadata(old, block)
    }

    pub(crate) fn direct_written(&mut self, direct: u32, block: u32) -> io::Result<()> {
        let old = self.index.direct_page_written(direct, block);
        self.segments.move_metadata(old, block)
    }

    pub(crate) fn indirect_written(&mut self, indirect: u32, block: u32) -> io::Result<()> {
        let old = self.index.indirect_page_written(indirect, block);
        self.segments.move_metadata(old, block)
    }

    /// The segment table's view of the head cursors must match the heads themselves.
    pub(crate) fn check_heads(&self) -> io::Result<()> {
        for head in self.heads {
            let entry = self.segments.entry(head.segment)?;
            if entry.head != Some(head.id) {
                return Err(invalid_data("head cursor disagrees with the segment table"));
            }
        }
        Ok(())
    }
}
