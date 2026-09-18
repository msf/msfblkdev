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
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for (page_id, entries) in cache.dirty_pages() {
        let descriptor = &mut candidate.descriptor;
        let record = MappingPageRecord {
            header: RecordHeader {
                kind: RecordKind::MappingPage,
                volume_id: descriptor.volume_id,
                lsn: descriptor.checkpoint_lsn + 1,
                previous_boundary: descriptor.last_record_boundary,
                boundary: descriptor.last_record_boundary + 1,
            },
            page_id,
            // The codec owns its fixed arrays; cloning copies 3,840 stack bytes.
            entries: entries.clone(),
        };
        record.encode(geometry, &mut bytes.0)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("mapping-page-write-before-{page_id}"))?;
        storage::write_block(backing_io, backing, record.header.boundary, &bytes)?;
        backing_io.mapping_io.page_writes += 1;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("mapping-page-write-complete-{page_id}"))?;
        candidate.mapping_page_block[page_id as usize] = record.header.boundary;
        descriptor.checkpoint_lsn = record.header.lsn;
        descriptor.last_record_boundary = record.header.boundary;
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
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for block in 0..geometry.index_blocks {
        candidate.encode_block(block, &mut bytes.0)?;
        storage::write_block(
            backing_io,
            backing,
            candidate.descriptor.slot.body_start(geometry) + block,
            &bytes,
        )?;
        backing_io.mapping_io.index_bytes_written += BLOCK_SIZE as u64;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(&format!("checkpoint-body-block-complete-{block}"))?;
    }
    storage::sync(backing_io, backing)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT)?;
    candidate.descriptor.encode(&mut bytes.0)?;
    storage::write_block(
        backing_io,
        backing,
        candidate.descriptor.slot as u32,
        &bytes,
    )?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::DESCRIPTOR_WRITE_COMPLETE_FAILPOINT)?;
    storage::sync(backing_io, backing)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    crate::pause_at_test_failpoint(crate::DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT)?;
    Ok(())
}
