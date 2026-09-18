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
        let mut active = checkpoint_load::load(&mut backing_io, &file, 32)?;
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
            let (backing, original) = super::format_v3_load::populated_fixture(320 * 1024 + 1)?;
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
    let selected = checkpoint_load::load(&mut backing_io, &file, 32)?;
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
