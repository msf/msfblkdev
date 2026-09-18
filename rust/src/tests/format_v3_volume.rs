use super::*;
use crate::format_v3::{storage, volume};

#[test]
fn paged_metrics_distinguish_cache_lookups_snapshot_io_and_tail_scans() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(32 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 641 * BLOCK_SIZE as u64, None)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let initial = volume.mapping_stats();
    assert_eq!(initial.index_bytes, 12);
    assert_eq!(initial.io.page_reads, 0);
    assert_eq!(initial.io.page_writes, 0);
    assert_eq!(initial.io.index_bytes_written, 0);
    assert_eq!(initial.io.tail_blocks_read, 28);
    assert_eq!(initial.recovery_passes, 1);
    assert_eq!(initial.cache.counters.foreground.hit_rate(), None);
    assert!(
        initial
            .summary()
            .contains("mapping_foreground_hit_rate=unavailable")
    );
    let mut bytes = [0; BLOCK_SIZE];
    for lba in [0, 0, 320] {
        volume.read_block(lba, &mut bytes)?;
    }
    volume.write_block(320, &[1; BLOCK_SIZE])?;
    volume.read_block(640, &mut bytes)?;
    volume.read_block(320, &mut bytes)?;
    volume.write_block(320, &[2; BLOCK_SIZE])?;
    volume.flush()?;
    let stats = volume.mapping_stats();
    assert_eq!(stats.cache.counters.foreground.hits, 3);
    assert_eq!(stats.cache.counters.foreground.misses, 4);
    assert_eq!(stats.cache.counters.clean_evictions, 3);
    assert_eq!(stats.cache.counters.checkpoint_required, 1);
    assert_eq!(stats.pressure_checkpoints, 1);
    assert_eq!(stats.checkpoint_failures, 0);
    assert_eq!(stats.io.page_reads, 1);
    assert_eq!(stats.io.page_writes, 1);
    assert_eq!(stats.io.index_bytes_written, 4096);
    assert_eq!(stats.io.tail_blocks_read, 28);
    assert_eq!(stats.cache.dirty_slots, 1);
    assert!(
        stats
            .summary()
            .contains("mapping_foreground_hit_rate=0.428571")
    );
    drop(volume);

    let reopened = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let stats = reopened.mapping_stats();
    assert_eq!(stats.io.page_reads, 2);
    assert_eq!(stats.io.page_writes, 0);
    assert_eq!(stats.io.index_bytes_written, 0);
    assert_eq!(stats.io.tail_blocks_read, 27);
    assert_eq!(stats.recovery_passes, 2);
    assert_eq!(stats.cache.counters.recovery.misses, 1);
    assert_eq!(stats.cache.counters.foreground.misses, 0);
    reopened.close()
}

#[test]
fn paged_reads_checkpoint_dirty_pressure_and_retry_without_double_counting() -> io::Result<()> {
    let (backing, checkpoint) = super::format_v3_load::populated_fixture(641)?;
    super::format_v3_publish::pending_update(&backing, &checkpoint)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    assert_eq!(volume.volume_blocks(), 641);
    let recovered = volume.cache_snapshot();
    assert_eq!(recovered.dirty_slots, 1);
    assert_eq!(recovered.counters.recovery.misses, 1);
    assert_eq!(recovered.counters.foreground.misses, 0);
    let mut bytes = [0xff; BLOCK_SIZE];
    for (lba, expected) in [(1, 0), (640, 0x5a), (0, 0x9c)] {
        volume.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [expected; BLOCK_SIZE]);
    }
    let snapshot = volume.cache_snapshot();
    assert_eq!(
        (
            snapshot.counters.foreground.hits,
            snapshot.counters.foreground.misses
        ),
        (1, 2)
    );
    assert_eq!(snapshot.counters.clean_evictions, 2);
    assert_eq!(snapshot.counters.checkpoint_required, 1);
    assert_eq!(volume.pressure_checkpoints(), 1);
    assert_eq!(snapshot.dirty_slots, 0);
    volume.flush()?;
    volume.close()?;
    let mut reopened = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    reopened.read_block(0, &mut bytes)?;
    assert_eq!(bytes, [0x9c; BLOCK_SIZE]);
    assert_eq!(
        reopened.read_block(641, &mut bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(bytes, [0x9c; BLOCK_SIZE]);
    reopened.close()
}

#[test]
fn paged_zero_reads_cover_eviction_and_the_final_partial_page() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(32 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 641 * BLOCK_SIZE as u64, None)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let mut bytes = [0xff; BLOCK_SIZE];
    for lba in [0, 320, 640, 0] {
        volume.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [0; BLOCK_SIZE]);
    }
    let snapshot = volume.cache_snapshot();
    assert_eq!(snapshot.counters.foreground.misses, 4);
    assert_eq!(snapshot.counters.clean_evictions, 3);
    assert_eq!(volume.pressure_checkpoints(), 0);
    volume.close()
}

#[test]
fn paged_metadata_failure_preserves_output_and_latches_the_volume() -> io::Result<()> {
    let (backing, checkpoint) = super::format_v3_load::populated_fixture(321)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let mut bytes = [0xee; BLOCK_SIZE];
    volume.read_block(0, &mut bytes)?;
    bytes.fill(0xee);
    let reads_before = volume.mapping_stats().io.page_reads;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    file.write_all_at(
        &[0xff],
        u64::from(checkpoint.mapping_page_block[1]) * BLOCK_SIZE as u64 + 40,
    )?;
    assert_eq!(
        volume.read_block(320, &mut bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(bytes, [0xee; BLOCK_SIZE]);
    assert_eq!(volume.mapping_stats().io.page_reads, reads_before + 1);
    assert_eq!(volume.cache_snapshot().counters.clean_evictions, 0);
    assert_eq!(volume.cache_snapshot().resident_slots, 1);
    assert_eq!(
        volume.read_block(0, &mut bytes).unwrap_err().kind(),
        io::ErrorKind::Other
    );
    assert!(volume.flush().is_err());
    assert!(volume.close().is_err());
    Ok(())
}

#[test]
fn paged_open_rejects_version_one_before_tail_sanitation() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(32 * BLOCK_SIZE as u64)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut original = vec![0xa5; 32 * BLOCK_SIZE];
    original[..2 * BLOCK_SIZE].copy_from_slice(include_bytes!("fixtures/v1-empty-roots.bin"));
    file.write_all_at(&original, 0)?;
    let error = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let mut after = vec![0; original.len()];
    file.read_exact_at(&mut after, 0)?;
    assert_eq!(after, original);
    Ok(())
}
