use super::*;
use crate::format_v3::{
    checkpoint_load, checkpoint_publish,
    mapping_page::{MappingPageRecord, PageExpectation},
};
use crate::io_accounting::{Reader, Writer};
use crate::mapping_cache::sizing::Sizing;

fn fixture(index_blocks: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(1024 * BLOCK_SIZE as u64)?;
    format(
        &backing.0,
        u64::from(index_blocks) * 1024 * 320 * BLOCK_SIZE as u64,
    )?;
    Ok(backing)
}

fn options(pages: u32) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: u64::from(pages) * BLOCK_SIZE as u64,
        cache_budget_bytes: Some(
            Sizing::for_capacity(pages as usize)
                .unwrap()
                .allocated_bytes as u64,
        ),
        ..VolumeOpenOptions::default()
    }
}

fn dirty(volume: &mut Volume, pages: u32) -> io::Result<()> {
    for page in 0..pages {
        volume.write_block(page * 320, &[page as u8 + 1; BLOCK_SIZE])?;
    }
    volume.flush()
}

fn verify(backing: &TemporaryBacking, pages: u32) -> io::Result<()> {
    let mut volume = open_with_options(&backing.0, options(pages))?;
    for page in 0..pages {
        let mut bytes = [0; BLOCK_SIZE];
        volume.read_block(page * 320, &mut bytes)?;
        assert_eq!(bytes, [page as u8 + 1; BLOCK_SIZE]);
    }
    volume.close()
}

#[test]
fn checkpoint_batches_preserve_records_and_exact_io_at_chunk_boundaries() -> io::Result<()> {
    for (pages, index_blocks) in [(1, 1), (2, 1), (31, 1), (32, 1), (33, 32), (65, 33)] {
        let backing = fixture(index_blocks)?;
        let ledger = TemporaryBacking::new()?;
        // SAFETY: the private ledger outlives all handles and has no external writers.
        let reader = unsafe { Reader::create(&ledger.0, 128 << 20)? };
        // SAFETY: same private ledger lifetime as above.
        let writer = unsafe { Writer::open(&ledger.0)? };
        let mut volume = open_with_accounting(&backing.0, options(pages), writer)?;
        dirty(&mut volume, pages)?;
        let tail = volume.mapping.tail;
        let old = volume.mapping.checkpoint.descriptor;
        let before = reader.snapshot()?;
        volume.close()?;
        let after = reader.snapshot()?;
        assert_eq!(
            after.write_completed_operations - before.write_completed_operations,
            u64::from(pages.div_ceil(32) + index_blocks.div_ceil(32) + 1)
        );
        assert_eq!(
            after.write_completed_bytes - before.write_completed_bytes,
            u64::from(pages + index_blocks + 1) * BLOCK_SIZE as u64
        );
        assert_eq!(
            after.flush_completed_operations - before.flush_completed_operations,
            3
        );
        let file = backing.reopen()?;
        let mut io = BackingIo::new(None)?;
        let (checkpoint, _) = checkpoint_load::load(&mut io, &file, 1024)?;
        assert_eq!(checkpoint.descriptor.generation, old.generation + 1);
        assert_eq!(
            checkpoint.descriptor.checkpoint_lsn,
            tail.lsn + u64::from(pages)
        );
        let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
        for page in 0..pages {
            let boundary = checkpoint.mapping_page_block[page as usize];
            read_block_exact(&mut io, &file, boundary, &mut bytes)?;
            let record = MappingPageRecord::decode(
                &bytes.0,
                &PageExpectation {
                    geometry: checkpoint.descriptor.geometry()?,
                    volume_id: old.volume_id,
                    page_id: page,
                    boundary,
                    maximum_lsn: checkpoint.descriptor.checkpoint_lsn,
                },
            )?;
            assert_eq!(record.header.lsn, tail.lsn + u64::from(page) + 1);
            assert_eq!(record.header.previous_boundary, tail.boundary + page);
            assert_eq!(record.header.boundary, tail.boundary + page + 1);
        }
        verify(&backing, pages)?;
    }
    Ok(())
}

#[test]
fn short_checkpoint_batches_latch_pressure_failure_and_keep_dirty_pages() -> io::Result<()> {
    for (index_failure, prefix_bytes) in [
        (false, 0),
        (false, 8 * BLOCK_SIZE),
        (false, 8 * BLOCK_SIZE + 512),
        (true, 0),
    ] {
        let backing = fixture(33)?;
        let mut volume = open_with_options(&backing.0, options(65))?;
        dirty(&mut volume, 65)?;
        let active = volume.mapping.checkpoint.descriptor;
        let tail = volume.mapping.tail;
        let at = if index_failure {
            active.slot.other().body_start(active.geometry()?) + 32
        } else {
            tail.boundary + 33
        };
        volume.inject_metadata_short_write(at, prefix_bytes);
        // A new page needs a victim; all 65 cache slots are dirty.
        assert!(volume.read_block(65 * 320, &mut [0; BLOCK_SIZE]).is_err());
        assert!(volume.failed);
        assert_eq!(volume.mapping.checkpoint.descriptor, active);
        assert_eq!(volume.mapping.tail, tail);
        assert_eq!(volume.mapping_stats().cache.dirty_slots, 65);
        assert_eq!(volume.mapping_stats().checkpoint_failures, 1);
        assert_eq!(
            volume.mapping_stats().io.page_writes,
            if index_failure { 65 } else { 32 }
        );
        assert_eq!(
            volume.mapping_stats().io.index_bytes_written,
            if index_failure {
                32 * BLOCK_SIZE as u64
            } else {
                0
            }
        );
        assert!(volume.write_block(0, &[255; BLOCK_SIZE]).is_err());
        assert!(volume.flush().is_err());
        assert!(volume.close().is_err());
        if prefix_bytes.is_multiple_of(BLOCK_SIZE) {
            verify(&backing, 65)?;
        } else {
            // The independent header checksum is at byte 3880 and was not written.
            // Existing recovery must fail closed instead of skipping unknown framing.
            let before = std::fs::read(&backing.0)?;
            let error = open_with_options(&backing.0, options(65)).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&backing.0)?, before);
            let (selected, _) =
                checkpoint_load::load(&mut BackingIo::new(None)?, &backing.reopen()?, 1024)?;
            assert_eq!(selected.descriptor, active);
        }
    }
    Ok(())
}

#[test]
fn missing_snapshot_blocks_do_not_publish_later_out_of_order_records() -> io::Result<()> {
    for missing in [0, 7, 31, 32, 64] {
        let backing = fixture(1)?;
        let mut volume = open_with_options(&backing.0, options(65))?;
        dirty(&mut volume, 65)?;
        let tail = volume.mapping.tail;
        let mut candidate = volume.mapping.checkpoint.try_clone()?;
        candidate.descriptor.checkpoint_lsn = tail.lsn;
        candidate.descriptor.last_record_boundary = tail.boundary;
        checkpoint_publish::append_snapshots(
            &mut BackingIo::new(None)?,
            &volume.backing,
            &volume.mapping.cache,
            &mut candidate,
        )?;
        drop(volume);
        // Retain the later completed records while leaving a hole in the batch.
        let file = OpenOptions::new().write(true).open(&backing.0)?;
        file.write_all_at(
            &[0; BLOCK_SIZE],
            u64::from(tail.boundary + 1 + missing) * BLOCK_SIZE as u64,
        )?;
        file.sync_all()?;
        verify(&backing, 65)?;
    }
    Ok(())
}

#[test]
fn batched_checkpoint_survives_sigkill_before_root_publication() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        return open_with_options(PathBuf::from(path), options(65))?.close();
    }
    let boundaries = [
        "mapping-page-write-before-32",
        "mapping-page-write-complete-31",
        "mapping-page-write-complete-63",
        "mapping-page-write-complete-64",
        "checkpoint-body-block-complete-31",
        "checkpoint-body-block-complete-32",
        DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
        DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
    ];
    for boundary in boundaries {
        for _ in 0..configured_crash_test_plan(boundaries.len() as u64)?.repetitions {
            let backing = fixture(33)?;
            let mut volume = open_with_options(&backing.0, options(65))?;
            dirty(&mut volume, 65)?;
            drop(volume);
            sigkill_child_at_handshake(
                "tests::checkpoint_batches::batched_checkpoint_survives_sigkill_before_root_publication",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            verify(&backing, 65)?;
        }
    }
    Ok(())
}
