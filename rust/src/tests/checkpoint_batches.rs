use super::*;
use crate::format_v4::{
    checkpoint_load,
    mapping_page::{MappingPageRecord, PageExpectation},
};
use crate::io_accounting::{Reader, Writer};
use crate::mapping_cache::sizing::Sizing;

/// Eight 512-block segments: one publication of 65 snapshots stays inside segment 0.
fn fixture(pages: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, pages * 336, 8, 512)?;
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
        volume.write_block(page * 336, &[page as u8 + 1; BLOCK_SIZE])?;
    }
    volume.flush()
}

fn verify(backing: &TemporaryBacking, pages: u32) -> io::Result<()> {
    let mut volume = open_with_options(&backing.0, options(pages))?;
    for page in 0..pages {
        let mut bytes = [0; BLOCK_SIZE];
        volume.read_block(page * 336, &mut bytes)?;
        assert_eq!(bytes, [page as u8 + 1; BLOCK_SIZE]);
    }
    volume.close()
}

fn load(backing: &TemporaryBacking) -> io::Result<checkpoint_load::Loaded> {
    let file = backing.reopen()?;
    checkpoint_load::load(
        &mut BackingIo::new(None)?,
        &file,
        file.metadata()?.len() / BLOCK_SIZE as u64,
        |geometry| geometry.segment_count,
    )
}

#[test]
fn checkpoint_batches_preserve_records_and_exact_io_at_chunk_boundaries() -> io::Result<()> {
    for pages in [1, 2, 31, 32, 33, 65] {
        let backing = fixture(pages)?;
        let ledger = TemporaryBacking::new()?;
        // SAFETY: the private ledger outlives all handles and has no external writers.
        let reader = unsafe { Reader::create(&ledger.0, 128 << 20)? };
        // SAFETY: same private ledger lifetime as above.
        let writer = unsafe { Writer::open(&ledger.0)? };
        let mut volume = open_with_accounting(&backing.0, options(pages), writer)?;
        dirty(&mut volume, pages)?;
        let tail = user_position(&volume);
        let old_lsn = volume.log.checkpoint_lsn();
        let volume_id = volume.log.volume_id;
        let before = reader.snapshot()?;
        volume.close()?;
        let after = reader.snapshot()?;
        // Snapshot batches of 32, one direct page, one descriptor; no segment changed state.
        assert_eq!(
            after.write_completed_operations - before.write_completed_operations,
            u64::from(pages.div_ceil(32) + 2)
        );
        assert_eq!(
            after.write_completed_bytes - before.write_completed_bytes,
            u64::from(pages + 2) * BLOCK_SIZE as u64
        );
        assert_eq!(
            after.flush_completed_operations - before.flush_completed_operations,
            2
        );
        let loaded = load(&backing)?;
        assert!(loaded.descriptor.checkpoint_lsn > old_lsn);
        assert_eq!(
            loaded.descriptor.checkpoint_lsn,
            tail.lsn + u64::from(pages) + 1
        );
        let file = backing.reopen()?;
        let mut io = BackingIo::new(None)?;
        let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
        for page in 0..pages {
            let boundary = loaded.log.index.snapshot_block(page);
            read_block_exact(&mut io, &file, boundary, &mut bytes)?;
            let record = MappingPageRecord::decode(
                &bytes.0,
                &PageExpectation {
                    geometry: loaded.log.geometry,
                    volume_id,
                    page_id: page,
                    boundary,
                    maximum_lsn: loaded.descriptor.checkpoint_lsn,
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
        let backing = fixture(66)?;
        let mut volume = open_with_options(&backing.0, options(65))?;
        dirty(&mut volume, 65)?;
        let active_lsn = volume.log.checkpoint_lsn();
        let tail = user_position(&volume);
        // The second snapshot batch starts 33 blocks after the tail; the direct page follows
        // all 65 snapshots.
        let at = if index_failure {
            tail.boundary + 66
        } else {
            tail.boundary + 33
        };
        volume.inject_metadata_short_write(at, prefix_bytes);
        // A new page needs a victim; all 65 cache slots are dirty.
        assert!(volume.read_block(65 * 336, &mut [0; BLOCK_SIZE]).is_err());
        assert!(volume.failed);
        assert_eq!(volume.log.checkpoint_lsn(), active_lsn);
        assert_eq!(volume.mapping_stats().cache.dirty_slots, 65);
        assert_eq!(volume.mapping_stats().checkpoint_failures, 1);
        assert_eq!(
            volume.mapping_stats().io.page_writes,
            if index_failure { 65 } else { 32 }
        );
        assert_eq!(volume.mapping_stats().io.index_bytes_written, 0);
        assert!(volume.write_block(0, &[255; BLOCK_SIZE]).is_err());
        assert!(volume.flush().is_err());
        assert!(volume.close().is_err());
        if prefix_bytes.is_multiple_of(BLOCK_SIZE) {
            verify(&backing, 65)?;
        } else {
            // The independent header checksum at byte 4072 was not written: recognizable
            // damaged metadata fails closed instead of being skipped.
            let before = std::fs::read(&backing.0)?;
            let error = open_with_options(&backing.0, options(65)).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&backing.0)?, before);
            assert_eq!(load(&backing)?.descriptor.checkpoint_lsn, active_lsn);
        }
    }
    Ok(())
}

#[test]
fn missing_snapshot_blocks_do_not_publish_later_out_of_order_records() -> io::Result<()> {
    for missing in [0, 7, 31, 32, 64] {
        let backing = fixture(65)?;
        let mut volume = open_with_options(&backing.0, options(65))?;
        dirty(&mut volume, 65)?;
        let tail = user_position(&volume);
        let mut io = BackingIo::new(None)?;
        volume
            .log
            .append_snapshots(&mut io, &volume.backing, &mut volume.mapping.cache)?;
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
        "direct-page-write-complete-0",
        "checkpoint-records-fsync-complete",
        DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
        DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
    ];
    for boundary in boundaries {
        for _ in 0..configured_crash_test_plan(boundaries.len() as u64)?.repetitions {
            let backing = fixture(65)?;
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
