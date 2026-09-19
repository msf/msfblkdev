use super::{
    RecordHeader, RecordKind, index::Checkpoint, mapping_page::MappingPageRecord,
    reservation::ensure_space, storage,
};
use crate::{
    AlignedBlock, BLOCK_SIZE, accounted_io::BackingIo, invalid_input, mapping_cache::MappingCache,
};
use std::{fs::File, io};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LogPosition {
    pub(crate) lsn: u64,
    pub(crate) boundary: u32,
}

/// The caller must stop using the volume on a backing-I/O failure.
pub(crate) fn publish(
    backing_io: &mut BackingIo,
    backing: &File,
    active: &mut Checkpoint,
    cache: &mut MappingCache,
    tail: LogPosition,
) -> io::Result<LogPosition> {
    let mut candidate = prepare(active, cache, tail)?;
    append_snapshots(backing_io, backing, cache, &mut candidate)?;
    storage::sync(backing_io, backing)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::LOG_FSYNC_COMPLETE_FAILPOINT)?;
    persist_root(backing_io, backing, &mut candidate)?;
    let published = LogPosition {
        lsn: candidate.descriptor.checkpoint_lsn,
        boundary: candidate.descriptor.last_record_boundary,
    };
    *active = candidate;
    cache.checkpoint_published();
    Ok(published)
}

fn prepare(active: &Checkpoint, cache: &MappingCache, tail: LogPosition) -> io::Result<Checkpoint> {
    let descriptor = active.descriptor;
    let geometry = descriptor.geometry()?;
    assert_eq!(
        active.mapping_page_block.len(),
        geometry.page_count as usize,
        "validated active index"
    );
    let snapshots = cache.dirty_pages().count() as u64;
    if tail.lsn < descriptor.checkpoint_lsn
        || tail.boundary < descriptor.last_record_boundary
        || tail.lsn - descriptor.checkpoint_lsn
            > u64::from(tail.boundary - descriptor.last_record_boundary)
        || (tail.lsn == descriptor.checkpoint_lsn)
            != (tail.boundary == descriptor.last_record_boundary)
    {
        return Err(invalid_input(
            "checkpoint tail precedes or contradicts active root",
        ));
    }
    ensure_space(tail.boundary, geometry.backing_blocks, snapshots)?;
    tail.lsn
        .checked_add(snapshots)
        .ok_or_else(|| invalid_input("checkpoint LSN exhausted"))?;
    let generation = descriptor.next_generation()?;
    let mut candidate = active.try_clone()?;
    candidate.descriptor.slot = descriptor.slot.other();
    candidate.descriptor.generation = generation;
    candidate.descriptor.checkpoint_lsn = tail.lsn;
    candidate.descriptor.last_record_boundary = tail.boundary;
    Ok(candidate)
}

pub(crate) fn append_snapshots(
    backing_io: &mut BackingIo,
    backing: &File,
    cache: &MappingCache,
    candidate: &mut Checkpoint,
) -> io::Result<()> {
    let geometry = candidate.descriptor.geometry()?;
    let mut blocks = [const { AlignedBlock([0; BLOCK_SIZE]) }; storage::CHECKPOINT_BATCH_BLOCKS];
    let mut page_ids = [0; storage::CHECKPOINT_BATCH_BLOCKS];
    let mut count = 0;
    for (page_id, entries) in cache.dirty_pages() {
        let descriptor = candidate.descriptor;
        let previous = descriptor.last_record_boundary + count as u32;
        let record = MappingPageRecord {
            header: RecordHeader {
                kind: RecordKind::MappingPage,
                volume_id: descriptor.volume_id,
                lsn: descriptor.checkpoint_lsn + count as u64 + 1,
                previous_boundary: previous,
                boundary: previous + 1,
            },
            page_id,
            // The codec owns its fixed arrays; cloning copies 4,032 stack bytes.
            entries: entries.clone(),
        };
        record.encode(geometry, &mut blocks[count].0)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("mapping-page-write-before-{page_id}"))?;
        page_ids[count] = page_id;
        count += 1;
        if count == blocks.len() {
            append_batch(backing_io, backing, candidate, &blocks, &page_ids)?;
            count = 0;
        }
    }
    if count != 0 {
        append_batch(
            backing_io,
            backing,
            candidate,
            &blocks[..count],
            &page_ids[..count],
        )?;
    }
    Ok(())
}

fn append_batch(
    backing_io: &mut BackingIo,
    backing: &File,
    candidate: &mut Checkpoint,
    blocks: &[AlignedBlock],
    page_ids: &[u32],
) -> io::Result<()> {
    assert_eq!(blocks.len(), page_ids.len());
    storage::write_blocks(
        backing_io,
        backing,
        candidate.descriptor.last_record_boundary + 1,
        blocks,
    )?;
    backing_io.mapping_io.page_writes += blocks.len() as u64;
    for &page_id in page_ids {
        let descriptor = &mut candidate.descriptor;
        descriptor.last_record_boundary += 1;
        descriptor.checkpoint_lsn += 1;
        candidate.mapping_page_block[page_id as usize] = descriptor.last_record_boundary;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("mapping-page-write-complete-{page_id}"))?;
    }
    Ok(())
}

pub(crate) fn persist_root(
    backing_io: &mut BackingIo,
    backing: &File,
    candidate: &mut Checkpoint,
) -> io::Result<()> {
    let geometry = candidate.descriptor.geometry()?;
    candidate.descriptor.index_checksum = candidate.checksum()?;
    let mut blocks = [const { AlignedBlock([0; BLOCK_SIZE]) }; storage::CHECKPOINT_BATCH_BLOCKS];
    for start in (0..geometry.index_blocks).step_by(blocks.len()) {
        let count = (geometry.index_blocks - start).min(blocks.len() as u32) as usize;
        for (offset, block) in blocks[..count].iter_mut().enumerate() {
            candidate.encode_block(start + offset as u32, &mut block.0)?;
        }
        storage::write_blocks(
            backing_io,
            backing,
            candidate.descriptor.slot.body_start(geometry) + start,
            &blocks[..count],
        )?;
        backing_io.mapping_io.index_bytes_written += (count * BLOCK_SIZE) as u64;
        #[cfg(any(test, feature = "test-failpoints"))]
        for block in start..start + count as u32 {
            crate::pause_at_test_failpoint(&format!("checkpoint-body-block-complete-{block}"))?;
        }
    }
    storage::sync(backing_io, backing)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT)?;
    candidate.descriptor.encode(&mut blocks[0].0)?;
    storage::write_blocks(
        backing_io,
        backing,
        candidate.descriptor.slot as u32,
        &blocks[..1],
    )?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::DESCRIPTOR_WRITE_COMPLETE_FAILPOINT)?;
    storage::sync(backing_io, backing)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT)?;
    Ok(())
}
