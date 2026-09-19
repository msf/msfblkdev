use super::*;
use crate::format_v3::{
    checkpoint_load, checkpoint_publish, data_record::DataRecord, index::Checkpoint,
};

#[test]
fn checkpoint_publication_survives_sigkill_at_each_boundary() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let backing = TemporaryBacking(path.into());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&backing.0)?;
        let mut backing_io = BackingIo::new(None)?;
        let (mut active, _) = checkpoint_load::load(&mut backing_io, &file, 32)?;
        let (mut cache, tail) = super::format_v3_publish::pending_update(&backing, &active)?;
        crate::format_v3::storage::sync(&mut backing_io, &file)?;
        checkpoint_publish::publish(&mut backing_io, &file, &mut active, &mut cache, tail)?;
        return Err(io::Error::other("checkpoint failpoint did not pause child"));
    }

    let boundaries = [
        ("mapping-page-write-before-0", false),
        ("mapping-page-write-complete-0", false),
        (LOG_FSYNC_COMPLETE_FAILPOINT, false),
        ("checkpoint-body-block-complete-0", false),
        ("checkpoint-body-block-complete-1", false),
        (CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT, false),
        (DESCRIPTOR_WRITE_COMPLETE_FAILPOINT, true),
        (DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT, true),
    ];
    let plan = configured_crash_test_plan(boundaries.len() as u64)?;
    // This small matrix runs every boundary in smoke mode too.
    for (failpoint, published) in boundaries {
        for _ in 0..plan.repetitions {
            let (backing, original) = super::format_v3_load::populated_fixture(336 * 1024 + 1)?;
            let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
            file.sync_all()?;
            sigkill_child_at_handshake(
                "tests::format_v3_crash::checkpoint_publication_survives_sigkill_at_each_boundary",
                &backing.0,
                failpoint,
                Some(failpoint),
            )?;
            verify_surviving_root_and_tail(&backing, &original, published)?;
        }
    }
    Ok(())
}

fn verify_surviving_root_and_tail(
    backing: &TemporaryBacking,
    original: &Checkpoint,
    published: bool,
) -> io::Result<()> {
    let file = backing.reopen()?;
    let mut backing_io = BackingIo::new(None)?;
    let (selected, _) = checkpoint_load::load(&mut backing_io, &file, 32)?;
    let old = original.descriptor;
    let geometry = old.geometry()?;
    assert_eq!(
        selected.descriptor.generation,
        old.generation + u64::from(published)
    );
    assert_eq!(
        selected.descriptor.checkpoint_lsn,
        old.checkpoint_lsn + if published { 2 } else { 0 }
    );
    assert_eq!(
        selected.mapping_page_block[1024],
        original.mapping_page_block[1024]
    );
    if published {
        assert_eq!(selected.mapping_page_block[0], old.last_record_boundary + 3);
    } else {
        assert_eq!(selected.mapping_page_block, original.mapping_page_block);
    }
    let physical = old.last_record_boundary + 1;
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(&mut backing_io, &file, physical, &mut bytes)?;
    assert_eq!(bytes.0, [0x9c; BLOCK_SIZE]);
    let checksum = payload_checksum(old.volume_id, 0, physical, &bytes.0);
    read_block_exact(&mut backing_io, &file, physical + 1, &mut bytes)?;
    let data = DataRecord::decode(&bytes.0, geometry, old.volume_id)?;
    assert!(
        data.header
            .follows(old.checkpoint_lsn, old.last_record_boundary)
    );
    assert_eq!(data.header.boundary, physical + 1);
    assert_eq!(data.lba_ids[0], 0);
    assert_eq!(data.checksums[0], checksum);
    Ok(())
}

#[test]
fn bounded_recovery_survives_sigkill_at_each_publication_boundary() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        return recovery_child(&PathBuf::from(path));
    }
    let boundaries = [
        ("mapping-page-write-before-0", false),
        ("mapping-page-write-complete-0", false),
        ("mapping-page-write-before-1", false),
        ("mapping-page-write-complete-1", false),
        ("recovery-snapshots-fsync-complete", false),
        ("checkpoint-body-block-complete-0", false),
        (CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT, false),
        (DESCRIPTOR_WRITE_COMPLETE_FAILPOINT, true),
        (DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT, true),
    ];
    for (failpoint, published) in boundaries {
        for _ in 0..configured_crash_test_plan(boundaries.len() as u64)?.repetitions {
            let (backing, original) = super::format_v3_recovery::fixture(1024)?;
            sigkill_child_at_handshake(
                "tests::format_v3_crash::bounded_recovery_survives_sigkill_at_each_publication_boundary",
                &backing.0,
                failpoint,
                Some(failpoint),
            )?;
            let mut backing_io = BackingIo::new(None)?;
            let (selected, _) = checkpoint_load::load(&mut backing_io, &backing.reopen()?, 1024)?;
            assert_eq!(selected.descriptor.generation, u64::from(published));
            verify_complete_recovery(&backing, original)?;
        }
    }
    Ok(())
}

#[test]
fn repeated_recovery_crashes_do_not_turn_staged_snapshots_into_writes() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        return recovery_child(&PathBuf::from(path));
    }
    let (backing, original) = super::format_v3_recovery::fixture(1024)?;
    for _ in 0..3 {
        sigkill_child_at_handshake(
            "tests::format_v3_crash::repeated_recovery_crashes_do_not_turn_staged_snapshots_into_writes",
            &backing.0,
            "mapping-page-write-complete-0",
            Some("mapping-page-write-complete-0"),
        )?;
        assert_eq!(
            checkpoint_load::load(&mut BackingIo::new(None)?, &backing.reopen()?, 1024)?
                .0
                .descriptor
                .generation,
            0
        );
    }
    let tail = verify_complete_recovery(&backing, original)?;
    assert_eq!(tail.lsn, 6);
    assert_eq!(tail.boundary, 347);
    Ok(())
}

fn recovery_child(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let mut backing_io = BackingIo::new(None)?;
    let (mut active, _) = checkpoint_load::load(&mut backing_io, &file, 1024)?;
    crate::format_v3::recovery::recover(&mut backing_io, &file, &mut active, Some(1))?;
    Err(io::Error::other("recovery failpoint did not pause child"))
}

fn verify_complete_recovery(
    backing: &TemporaryBacking,
    original: Checkpoint,
) -> io::Result<crate::format_v3::checkpoint_publish::LogPosition> {
    use crate::format_v3::{
        mapping_page::{MappingPageRecord, PageExpectation},
        recovery,
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&backing.0)?;
    let mut backing_io = BackingIo::new(None)?;
    let (mut active, _) = checkpoint_load::load(&mut backing_io, &file, 1024)?;
    let recovered = recovery::recover(&mut backing_io, &file, &mut active, Some(1))?;
    assert_eq!(active.descriptor.generation, 1);
    assert_eq!(recovered.cache.snapshot().dirty_slots, 0);
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for page_id in 0..2 {
        let boundary = active.mapping_page_block[page_id as usize];
        read_block_exact(&mut backing_io, &file, boundary, &mut bytes)?;
        let page = MappingPageRecord::decode(
            &bytes.0,
            &PageExpectation {
                geometry: active.descriptor.geometry()?,
                volume_id: active.descriptor.volume_id,
                page_id,
                boundary,
                maximum_lsn: active.descriptor.checkpoint_lsn,
            },
        )?;
        for index in 0..336 {
            let lba = page_id * 336 + index as u32;
            let physical = if lba < 338 { 4 + lba } else { 0 };
            let checksum = if lba < 338 {
                payload_checksum(
                    original.descriptor.volume_id,
                    lba,
                    physical,
                    &[0; BLOCK_SIZE],
                )
            } else {
                0
            };
            assert_eq!(page.entries.physical_addr[index], physical);
            assert_eq!(page.entries.payload_checksum[index], checksum);
            if physical != 0 {
                read_block_exact(&mut backing_io, &file, physical, &mut bytes)?;
                assert_eq!(bytes.0, [0; BLOCK_SIZE]);
            }
        }
    }
    read_block_exact(&mut backing_io, &file, 342, &mut bytes)?;
    let data = DataRecord::decode(
        &bytes.0,
        original.descriptor.geometry()?,
        original.descriptor.volume_id,
    )?;
    assert_eq!(data.header.lsn, 1);
    assert_eq!(data.header.payload_count()?, 338);
    Ok(recovered.tail)
}
