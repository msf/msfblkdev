//! Checkpoint publication: snapshots, index pages, table pages, one barrier, the descriptor.
//!
//! Table pages append last so that they describe the allocation state the descriptor names,
//! including segments the publication itself opened (review finding T4).
use super::{
    HeadId, RecordHeader, RecordKind,
    descriptor::CheckpointDescriptor,
    geometry::{DIRECT_POINTERS, INDIRECT_POINTERS, TABLE_POINTERS},
    log::Log,
    mapping_page::MappingPageRecord,
    storage::{self, BATCH_BLOCKS},
};
use crate::{
    AlignedBlock, BLOCK_SIZE, accounted_io::BackingIo, invalid_data, mapping_cache::MappingCache,
};
use std::{fs::File, io};

/// Consecutive one-block metadata records staged for one positional write.
struct Batch {
    headers: [RecordHeader; BATCH_BLOCKS],
    ids: [u32; BATCH_BLOCKS],
    count: usize,
}

impl Batch {
    fn new(log: &Log) -> Self {
        Self {
            headers: [log.metadata_header(HeadId::User, RecordKind::SegmentTablePage);
                BATCH_BLOCKS],
            ids: [0; BATCH_BLOCKS],
            count: 0,
        }
    }

    /// The header for the next staged record on the User head.
    fn next_header(&self, log: &Log, kind: RecordKind) -> RecordHeader {
        let head = log.head(HeadId::User);
        let offset = self.count as u32;
        RecordHeader {
            kind,
            volume_id: log.volume_id,
            lsn: log.next_lsn + u64::from(offset),
            previous_boundary: if offset == 0 {
                head.last_boundary
            } else {
                head.next_block + offset - 1
            },
            boundary: head.next_block + offset,
        }
    }

    fn is_full(&self, log: &Log) -> bool {
        self.count == BATCH_BLOCKS || self.count as u32 == log.head(HeadId::User).room(log.geometry)
    }
}

impl Log {
    /// Appends every dirty snapshot on the User head and points the resident index at them.
    pub(crate) fn append_snapshots(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        cache: &mut MappingCache,
    ) -> io::Result<()> {
        let geometry = self.geometry;
        let mut batch = Batch::new(self);
        for (page_id, entries) in cache.sorted_dirty_pages() {
            if self.head(HeadId::User).room(geometry) == batch.count as u32 {
                self.flush_snapshots(io, backing, &mut batch)?;
                self.advance_head(io, backing, HeadId::User)?;
            }
            let header = batch.next_header(self, RecordKind::MappingPage);
            let record = MappingPageRecord {
                header,
                page_id,
                // The codec owns its fixed arrays; cloning copies 4,032 stack bytes.
                entries: entries.clone(),
            };
            record.encode(geometry, &mut self.scratch[batch.count].0)?;
            #[cfg(any(test, feature = "test-failpoints"))]
            crate::pause_at_indexed_failpoint("mapping-page-write-before", u64::from(page_id))?;
            batch.headers[batch.count] = header;
            batch.ids[batch.count] = page_id;
            batch.count += 1;
            if batch.is_full(self) {
                self.flush_snapshots(io, backing, &mut batch)?;
            }
        }
        self.flush_snapshots(io, backing, &mut batch)
    }

    fn flush_snapshots(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        batch: &mut Batch,
    ) -> io::Result<()> {
        if batch.count == 0 {
            return Ok(());
        }
        self.commit_metadata_batch(io, backing, HeadId::User, &batch.headers[..batch.count])?;
        io.mapping_io.page_writes += batch.count as u64;
        for index in 0..batch.count {
            self.snapshot_written(batch.ids[index], batch.headers[index].boundary)?;
            #[cfg(any(test, feature = "test-failpoints"))]
            crate::pause_at_indexed_failpoint(
                "mapping-page-write-complete",
                u64::from(batch.ids[index]),
            )?;
        }
        batch.count = 0;
        Ok(())
    }

    /// Appends dirty direct pages, then dirty indirect pages.
    fn append_index_pages(&mut self, io: &mut BackingIo, backing: &File) -> io::Result<()> {
        let geometry = self.geometry;
        let mut batch = Batch::new(self);
        for direct in 0..geometry.direct_pages {
            if !self.index.is_direct_dirty(direct) {
                continue;
            }
            if self.head(HeadId::User).room(geometry) == batch.count as u32 {
                self.flush_index_pages(io, backing, &mut batch, false)?;
                self.advance_head(io, backing, HeadId::User)?;
            }
            let header = batch.next_header(self, RecordKind::MappingIndexPage);
            self.index
                .encode_direct(header, direct)
                .encode(geometry, &mut self.scratch[batch.count].0)?;
            batch.headers[batch.count] = header;
            batch.ids[batch.count] = direct;
            batch.count += 1;
            if batch.is_full(self) {
                self.flush_index_pages(io, backing, &mut batch, false)?;
            }
        }
        self.flush_index_pages(io, backing, &mut batch, false)?;
        for indirect in 0..geometry.indirect_pages {
            if !self.index.is_indirect_dirty(indirect) {
                continue;
            }
            if self.head(HeadId::User).room(geometry) == batch.count as u32 {
                self.flush_index_pages(io, backing, &mut batch, true)?;
                self.advance_head(io, backing, HeadId::User)?;
            }
            let header = batch.next_header(self, RecordKind::MappingIndexPage);
            self.index
                .encode_indirect(header, indirect)
                .encode(geometry, &mut self.scratch[batch.count].0)?;
            batch.headers[batch.count] = header;
            batch.ids[batch.count] = indirect;
            batch.count += 1;
            if batch.is_full(self) {
                self.flush_index_pages(io, backing, &mut batch, true)?;
            }
        }
        self.flush_index_pages(io, backing, &mut batch, true)
    }

    fn flush_index_pages(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        batch: &mut Batch,
        indirect: bool,
    ) -> io::Result<()> {
        if batch.count == 0 {
            return Ok(());
        }
        self.commit_metadata_batch(io, backing, HeadId::User, &batch.headers[..batch.count])?;
        io.mapping_io.index_bytes_written += (batch.count * BLOCK_SIZE) as u64;
        for index in 0..batch.count {
            let (id, block) = (batch.ids[index], batch.headers[index].boundary);
            if indirect {
                self.indirect_written(id, block)?;
            } else {
                self.direct_written(id, block)?;
            }
            #[cfg(any(test, feature = "test-failpoints"))]
            crate::pause_at_indexed_failpoint(
                if indirect {
                    "indirect-page-write-complete"
                } else {
                    "direct-page-write-complete"
                },
                u64::from(id),
            )?;
        }
        batch.count = 0;
        Ok(())
    }

    /// Appends every dirty table page inside one segment, after making room for all of them.
    fn append_table_pages(&mut self, io: &mut BackingIo, backing: &File) -> io::Result<()> {
        let geometry = self.geometry;
        // Advancing dirties two more entries, so recount until the dirty set fits.
        loop {
            let dirty = self.segments.dirty_table_page_count();
            if self.head(HeadId::User).room(geometry) >= dirty {
                break;
            }
            self.advance_head(io, backing, HeadId::User)?;
        }
        let mut batch = Batch::new(self);
        for table in 0..geometry.table_pages {
            if !self.segments.table_dirty(table) {
                continue;
            }
            let header = batch.next_header(self, RecordKind::SegmentTablePage);
            self.segments
                .encode_table_page(header, table)?
                .encode(geometry, &mut self.scratch[batch.count].0)?;
            batch.headers[batch.count] = header;
            batch.ids[batch.count] = table;
            batch.count += 1;
            if batch.count == BATCH_BLOCKS {
                self.flush_table_pages(io, backing, &mut batch)?;
            }
        }
        self.flush_table_pages(io, backing, &mut batch)
    }

    fn flush_table_pages(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        batch: &mut Batch,
    ) -> io::Result<()> {
        if batch.count == 0 {
            return Ok(());
        }
        self.commit_metadata_batch(io, backing, HeadId::User, &batch.headers[..batch.count])?;
        io.mapping_io.index_bytes_written += (batch.count * BLOCK_SIZE) as u64;
        for index in 0..batch.count {
            self.segments
                .table_page_written(batch.ids[index], batch.headers[index].boundary)?;
            #[cfg(any(test, feature = "test-failpoints"))]
            crate::pause_at_indexed_failpoint(
                "table-page-write-complete",
                u64::from(batch.ids[index]),
            )?;
        }
        batch.count = 0;
        Ok(())
    }

    fn descriptor(&self) -> io::Result<CheckpointDescriptor> {
        let checkpoint_lsn = self
            .next_lsn
            .checked_sub(1)
            .ok_or_else(|| invalid_data("no record precedes the checkpoint"))?;
        if checkpoint_lsn <= self.checkpoint_lsn() {
            return Err(invalid_data("a publication must advance the log"));
        }
        let mut descriptor = CheckpointDescriptor {
            slot: self.active_slot.other(),
            volume_id: self.volume_id,
            volume_blocks: self.geometry.volume_blocks,
            backing_blocks: self.geometry.backing_blocks,
            checkpoint_lsn,
            blocks_per_segment: self.geometry.blocks_per_segment,
            head_boundary: [
                self.head(HeadId::User).last_boundary,
                self.head(HeadId::Relocation).last_boundary,
            ],
            direct_pages: [0; DIRECT_POINTERS],
            indirect_pages: [0; INDIRECT_POINTERS],
            table_pages: [0; TABLE_POINTERS],
        };
        self.index.fill_descriptor(&mut descriptor);
        for table in 0..self.geometry.table_pages {
            descriptor.table_pages[table as usize] = self.segments.table_block(table);
        }
        Ok(descriptor)
    }

    /// Completes a publication whose snapshots are already appended.
    pub(crate) fn complete_publication(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        cache: &mut MappingCache,
    ) -> io::Result<()> {
        self.append_index_pages(io, backing)?;
        self.append_table_pages(io, backing)?;
        storage::sync(io, backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("checkpoint-records-fsync-complete")?;
        let descriptor = self.descriptor()?;
        let mut block = AlignedBlock([0; BLOCK_SIZE]);
        descriptor.encode(&mut block.0)?;
        storage::write_blocks(
            io,
            backing,
            descriptor.slot.block(),
            std::slice::from_ref(&block),
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::DESCRIPTOR_WRITE_COMPLETE_FAILPOINT)?;
        storage::sync(io, backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT)?;
        self.active_slot = descriptor.slot;
        self.root_lsn[descriptor.slot as usize] = Some(descriptor.checkpoint_lsn);
        self.segments.checkpoint_published()?;
        cache.checkpoint_published();
        self.blocks_since_checkpoint = 0;
        Ok(())
    }

    /// Whether a publication would write anything.
    pub(crate) fn has_dirty_state(&self, cache: &MappingCache) -> bool {
        cache.dirty_pages().next().is_some()
            || self.index.dirty_page_count() != 0
            || self.segments.dirty_table_page_count() != 0
    }

    /// Publishes a checkpoint unless nothing changed. The caller latches the volume on error.
    pub(crate) fn publish(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        cache: &mut MappingCache,
    ) -> io::Result<bool> {
        if !self.has_dirty_state(cache) {
            return Ok(false);
        }
        self.append_snapshots(io, backing, cache)?;
        self.complete_publication(io, backing, cache)?;
        Ok(true)
    }
}
