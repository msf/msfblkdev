use super::*;
use crate::format_v3::{
    checkpoint_load,
    index::Checkpoint,
    mapping_page::{MappingPageRecord, PageExpectation},
    recovery, storage,
};

pub(super) fn fixture(blocks: u64) -> io::Result<(TemporaryBacking, Checkpoint)> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(blocks * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 640 * BLOCK_SIZE as u64, None)?;
    let (checkpoint, _) =
        checkpoint_load::load(&mut BackingIo::new(None)?, &backing.reopen()?, blocks)?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    super::format_v3_tail::write_zero_payload_record(&file, checkpoint.descriptor, 338)?;
    file.sync_all()?;
    Ok((backing, checkpoint))
}

fn direct_writer(backing: &TemporaryBacking) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&backing.0)
}

#[test]
fn bounded_recovery_matches_fast_path_and_preserves_its_replay_source() -> io::Result<()> {
    let (backing, mut active) = fixture(1024)?;
    let file = direct_writer(&backing)?;
    let mut backing_io = BackingIo::new(None)?;
    let fast = recovery::recover(&mut backing_io, &file, &mut active, Some(20_000))?;
    assert_eq!(fast.cache.snapshot().dirty_slots, 2);
    assert_eq!(fast.stats.tail_passes, 2);
    assert_eq!(fast.stats.newly_mapped_blocks, 338);
    assert_eq!(fast.stats.records_scanned, 2);
    assert_eq!(fast.stats.snapshot_writes, 0);
    assert_eq!(fast.stats.index_bytes, 0);
    assert_eq!(
        (fast.stats.lookups.hits, fast.stats.lookups.misses),
        (336, 2)
    );
    let expected: Vec<_> = fast
        .cache
        .dirty_pages()
        .map(|(id, entries)| (id, entries.clone()))
        .collect();
    let end = fast.tail;
    assert_eq!(backing_io.mapping_io.page_reads, 0);
    assert_eq!(backing_io.mapping_io.page_writes, 0);
    assert_eq!(backing_io.mapping_io.index_bytes_written, 0);
    drop(fast);
    assert_eq!(active.descriptor.generation, 0);
    let plain = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut before = vec![0; 339 * BLOCK_SIZE];
    plain.read_exact_at(&mut before, 4 * BLOCK_SIZE as u64)?;
    let bounded = recovery::recover(&mut backing_io, &file, &mut active, Some(1))?;
    assert_eq!(bounded.cache.snapshot().sizing.capacity, 1);
    assert_eq!(bounded.cache.snapshot().dirty_slots, 0);
    assert_eq!(bounded.stats.tail_passes, 3);
    assert_eq!(bounded.stats.newly_mapped_blocks, 338);
    assert_eq!(bounded.stats.records_scanned, 3);
    assert_eq!(bounded.stats.page_reads, 0);
    assert_eq!(bounded.stats.snapshot_writes, 2);
    assert_eq!(bounded.stats.index_bytes, BLOCK_SIZE as u64);
    assert_eq!(backing_io.mapping_io.page_writes, 2);
    assert_eq!(backing_io.mapping_io.index_bytes_written, BLOCK_SIZE as u64);
    assert_eq!(
        (bounded.stats.lookups.hits, bounded.stats.lookups.misses),
        (336, 2)
    );
    assert_eq!(bounded.tail.boundary, end.boundary + 2);
    assert_eq!(bounded.tail.lsn, end.lsn + 2);
    assert_eq!(active.descriptor.generation, 1);
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for (page_id, entries) in &expected {
        let boundary = active.mapping_page_block[*page_id as usize];
        read_block_exact(&mut backing_io, &file, boundary, &mut bytes)?;
        let mapping = MappingPageRecord::decode(
            &bytes.0,
            &PageExpectation {
                geometry: active.descriptor.geometry()?,
                volume_id: active.descriptor.volume_id,
                page_id: *page_id,
                boundary,
                maximum_lsn: active.descriptor.checkpoint_lsn,
            },
        )?;
        assert_eq!(&mapping.entries, entries);
    }
    let mut after = vec![0; before.len()];
    plain.read_exact_at(&mut after, 4 * BLOCK_SIZE as u64)?;
    assert_eq!(after, before);
    drop(bounded);
    let (mut reopened, live_blocks) = checkpoint_load::load(&mut backing_io, &file, 1024)?;
    assert_eq!(live_blocks, 338);
    assert_eq!(reopened.descriptor, active.descriptor);
    let unchanged = recovery::recover(&mut backing_io, &file, &mut reopened, Some(1))?;
    assert_eq!(unchanged.stats.tail_passes, 1);
    assert_eq!(unchanged.stats.newly_mapped_blocks, 0);
    assert_eq!(unchanged.stats.snapshot_writes, 0);
    assert_eq!(unchanged.stats.index_bytes, 0);
    drop(unchanged);
    let overwrite =
        super::format_v3_tail::write_zero_payload_record(&plain, reopened.descriptor, 1)?;
    let recovered = recovery::recover(&mut backing_io, &file, &mut reopened, Some(1))?;
    assert_eq!(recovered.stats.page_reads, 1);
    assert_eq!(recovered.stats.newly_mapped_blocks, 0);
    let (_, page) = recovered.cache.dirty_pages().next().unwrap();
    let mut expected_page = expected[0].1.clone();
    expected_page.physical_addr[0] = overwrite.header.previous_boundary + 1;
    expected_page.payload_checksum[0] = overwrite.checksums[0];
    assert_eq!(page, &expected_page);
    Ok(())
}

#[test]
fn recovery_space_exhaustion_leaves_every_backing_byte_and_root_unchanged() -> io::Result<()> {
    let (backing, mut active) = fixture(344)?;
    let descriptor = active.descriptor;
    let mut before = vec![0; 344 * BLOCK_SIZE];
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    file.read_exact_at(&mut before, 0)?;
    let error = recovery::recover(
        &mut BackingIo::new(None)?,
        &direct_writer(&backing)?,
        &mut active,
        Some(1),
    )
    .err()
    .unwrap();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(active.descriptor, descriptor);
    assert_eq!(active.mapping_page_block.as_ref(), &[0, 0]);
    let mut after = vec![0; before.len()];
    file.read_exact_at(&mut after, 0)?;
    assert_eq!(after, before);
    Ok(())
}

#[test]
fn bounded_recovery_rebuilds_an_older_root_after_snapshot_body_corruption() -> io::Result<()> {
    let (backing, checkpoint) = super::format_v3_load::populated_fixture(321)?;
    super::format_v3_publish::pending_update(&backing, &checkpoint)?;
    let plain = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    plain.write_all_at(
        &[0xff],
        u64::from(checkpoint.descriptor.last_record_boundary) * BLOCK_SIZE as u64 + 40,
    )?;
    plain.sync_all()?;
    let file = direct_writer(&backing)?;
    let mut backing_io = BackingIo::new(None)?;
    let (mut older, live_blocks) = checkpoint_load::load(&mut backing_io, &file, 32)?;
    assert_eq!(live_blocks, 0);
    assert_eq!(older.descriptor.generation, 0);
    let recovered = recovery::recover(&mut backing_io, &file, &mut older, Some(1))?;
    assert_eq!(recovered.tail.lsn, 5);
    assert_eq!(recovered.stats.newly_mapped_blocks, 2);
    assert_eq!(recovered.stats.records_scanned, 9);
    assert_eq!(recovered.stats.snapshot_writes, 2);
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for (page_id, physical) in [(0, 7), (1, 4)] {
        let boundary = older.mapping_page_block[page_id as usize];
        read_block_exact(&mut backing_io, &file, boundary, &mut bytes)?;
        let mapping = MappingPageRecord::decode(
            &bytes.0,
            &PageExpectation {
                geometry: older.descriptor.geometry()?,
                volume_id: older.descriptor.volume_id,
                page_id,
                boundary,
                maximum_lsn: older.descriptor.checkpoint_lsn,
            },
        )?;
        assert_eq!(mapping.entries.physical_addr[0], physical);
    }
    Ok(())
}
