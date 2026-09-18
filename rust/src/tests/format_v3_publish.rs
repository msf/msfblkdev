use super::*;
use crate::format_v3::{
    RecordHeader, RecordKind, checkpoint_load,
    checkpoint_publish::{self, LogPosition},
    data_record::DataRecord,
    index::Checkpoint,
    mapping_page::{MappingPageRecord, PageExpectation},
};
use crate::mapping_cache::{Access, Acquisition, MappingCache, MappingEntries};

pub(super) fn pending_update(
    backing: &TemporaryBacking,
    checkpoint: &Checkpoint,
) -> io::Result<(MappingCache, LogPosition)> {
    let geometry = checkpoint.descriptor.geometry()?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    let physical = checkpoint.descriptor.last_record_boundary + 1;
    let payload = [0x9c; BLOCK_SIZE];
    file.write_all_at(&payload, u64::from(physical) * BLOCK_SIZE as u64)?;
    let checksum = payload_checksum(checkpoint.descriptor.volume_id, 0, physical, &payload);
    let mut record = DataRecord {
        header: RecordHeader {
            kind: RecordKind::Data,
            volume_id: checkpoint.descriptor.volume_id,
            lsn: checkpoint.descriptor.checkpoint_lsn + 1,
            previous_boundary: physical - 1,
            boundary: physical + 1,
        },
        lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
        checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
    };
    record.checksums[0] = checksum;
    let mut bytes = [0; BLOCK_SIZE];
    record.encode(geometry, &mut bytes)?;
    file.write_all_at(&bytes, u64::from(physical + 1) * BLOCK_SIZE as u64)?;
    let mut cache = MappingCache::new(
        u64::from(geometry.volume_blocks) * BLOCK_SIZE as u64,
        Some(1),
    )
    .unwrap();
    let Acquisition::Vacant(page) = cache.acquire(0, Access::Foreground).unwrap() else {
        panic!("empty cache must have a vacant slot");
    };
    page.install(MappingEntries::zeroed())
        .unwrap()
        .update(0, physical, checksum)
        .unwrap();
    Ok((
        cache,
        LogPosition {
            lsn: record.header.lsn,
            boundary: record.header.boundary,
        },
    ))
}

#[test]
fn checkpoint_publish_persists_dirty_page_and_reuses_unchanged_pointers() -> io::Result<()> {
    let (backing, mut active) = super::format_v3_load::populated_fixture(321)?;
    let (mut cache, tail) = pending_update(&backing, &active)?;
    let descriptor = active.descriptor;
    let geometry = descriptor.geometry()?;
    let original_pointer = active.mapping_page_block[1];
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&backing.0)?;
    let mut backing_io = BackingIo::new(None)?;
    let mut original = [AlignedBlock([0; BLOCK_SIZE]), AlignedBlock([0; BLOCK_SIZE])];
    read_block_exact(
        &mut backing_io,
        &file,
        descriptor.slot as u32,
        &mut original[0],
    )?;
    read_block_exact(
        &mut backing_io,
        &file,
        descriptor.slot.body_start(geometry),
        &mut original[1],
    )?;
    let expected = cache.dirty_pages().next().unwrap().1.clone();
    let published =
        checkpoint_publish::publish(&mut backing_io, &file, &mut active, &mut cache, tail)?;
    assert_eq!(
        published,
        LogPosition {
            lsn: tail.lsn + 1,
            boundary: tail.boundary + 1
        }
    );
    assert_eq!(active.descriptor.generation, descriptor.generation + 1);
    assert_eq!(active.descriptor.slot, descriptor.slot.other());
    assert_eq!(
        active.mapping_page_block.as_ref(),
        &[published.boundary, original_pointer]
    );
    assert_eq!(cache.snapshot().dirty_slots, 0);
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for (block, old) in [descriptor.slot as u32, descriptor.slot.body_start(geometry)]
        .into_iter()
        .zip(original)
    {
        read_block_exact(&mut backing_io, &file, block, &mut bytes)?;
        assert_eq!(bytes.0, old.0);
    }
    read_block_exact(&mut backing_io, &file, published.boundary, &mut bytes)?;
    let mapping = MappingPageRecord::decode(
        &bytes.0,
        &PageExpectation {
            geometry,
            volume_id: descriptor.volume_id,
            page_id: 0,
            boundary: published.boundary,
            maximum_lsn: published.lsn,
        },
    )?;
    assert_eq!(mapping.entries, expected);
    let selected = checkpoint_load::load(&mut backing_io, &file, 32)?;
    assert_eq!(selected.descriptor, active.descriptor);
    assert_eq!(selected.mapping_page_block, active.mapping_page_block);
    let pointers = active.mapping_page_block.clone();
    assert_eq!(
        checkpoint_publish::publish(&mut backing_io, &file, &mut active, &mut cache, published)?,
        published
    );
    assert_eq!(active.mapping_page_block, pointers);
    assert_eq!(active.descriptor.generation, descriptor.generation + 2);
    Ok(())
}

#[test]
fn checkpoint_publish_rejects_exhausted_space_and_generation_before_writing() -> io::Result<()> {
    let (backing, mut active) = super::format_v3_load::populated_fixture(321)?;
    let (mut cache, tail) = pending_update(&backing, &active)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut before = vec![0; 32 * BLOCK_SIZE];
    file.read_exact_at(&mut before, 0)?;
    let mut backing_io = BackingIo::new(None)?;
    for (lsn, boundary) in [(1, 8), (3, 5), (5, 7), (2, 8), (3, 6), (3, 32)] {
        let error = checkpoint_publish::publish(
            &mut backing_io,
            &file,
            &mut active,
            &mut cache,
            LogPosition { lsn, boundary },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
    let error = checkpoint_publish::publish(
        &mut backing_io,
        &file,
        &mut active,
        &mut cache,
        LogPosition {
            boundary: 31,
            ..tail
        },
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(active.descriptor.generation, 1);
    active.descriptor.generation = u64::MAX;
    assert!(
        checkpoint_publish::publish(&mut backing_io, &file, &mut active, &mut cache, tail).is_err()
    );
    assert_eq!(active.descriptor.generation, u64::MAX);
    assert_eq!(cache.snapshot().dirty_slots, 1);
    let mut after = vec![0; before.len()];
    file.read_exact_at(&mut after, 0)?;
    assert_eq!(after, before);
    Ok(())
}
