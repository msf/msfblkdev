use super::*;
use crate::format_v3::{checkpoint_load, storage, volume};

fn fixture(logical: u64, blocks: u64) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(blocks * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, logical * BLOCK_SIZE as u64, None)?;
    Ok(backing)
}

#[test]
fn paged_buffer_groups_interleaved_writes_and_coalesces_before_pressure() -> io::Result<()> {
    let backing = fixture(961, 64)?;
    let options = VolumeOpenOptions {
        write_buffer_bytes: 8 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    let mut volume = volume::open(&backing.0, options, Some(1), None)?;
    for (lba, byte) in [(0, 1), (320, 2), (1, 3), (640, 4), (321, 5), (0, 6)] {
        volume.write_block(lba, &[byte; BLOCK_SIZE])?;
    }
    assert_eq!(volume.release_stats().records, 0);
    let mut bytes = [0; BLOCK_SIZE];
    volume.read_block(0, &mut bytes)?;
    assert_eq!(bytes, [6; BLOCK_SIZE]);
    assert_eq!(volume.cache_snapshot().counters.foreground.hits, 0);
    assert_eq!(volume.cache_snapshot().counters.foreground.misses, 0);
    volume.flush()?;
    assert_eq!(volume.release_stats().records, 3);
    assert_eq!(volume.release_stats().payloads, 5);
    assert_eq!(volume.pressure_checkpoints(), 2);
    assert_eq!(volume.cache_snapshot().dirty_slots, 1);
    drop(volume);
    let mut reopened = volume::open(&backing.0, options, Some(1), None)?;
    for (lba, byte) in [(0, 6), (320, 2), (1, 3), (640, 4), (321, 5), (960, 0)] {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [byte; BLOCK_SIZE]);
    }
    reopened.close()
}

#[test]
fn paged_full_page_release_is_one_record_without_runtime_allocations() -> io::Result<()> {
    let backing = fixture(320, 2048)?;
    let options = VolumeOpenOptions {
        write_buffer_bytes: 320 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    let mut volume = volume::open(&backing.0, options, Some(1), None)?;
    for lba in 0..319 {
        volume.write_block(lba, &[lba as u8; BLOCK_SIZE])?;
    }
    let (result, allocations) =
        super::allocations::measure(|| volume.write_block(319, &[63; BLOCK_SIZE]));
    result?;
    assert_eq!(allocations, 0);
    assert_eq!(volume.release_stats().records, 1);
    assert_eq!(volume.release_stats().payloads, 320);
    assert_eq!(volume.release_stats().full_releases, 1);
    assert_eq!(volume.release_stats().payload_histogram[5], 1);
    volume.close()?;
    let mut reopened = volume::open(&backing.0, options, Some(1), None)?;
    let mut bytes = [0; BLOCK_SIZE];
    for lba in 0..320 {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [lba as u8; BLOCK_SIZE]);
    }
    reopened.close()
}

#[test]
fn paged_admission_preserves_snapshot_space_and_rejects_before_buffering() -> io::Result<()> {
    let backing = fixture(641, 10)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    volume.write_block(0, &[1; BLOCK_SIZE])?;
    volume.write_block(320, &[2; BLOCK_SIZE])?;
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    let mut before = [0; 10 * BLOCK_SIZE];
    file.read_exact_at(&mut before, 0)?;
    let snapshot = volume.cache_snapshot();
    assert_eq!(
        volume
            .write_block(640, &[3; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(volume.cache_snapshot(), snapshot);
    let mut after = [0; 10 * BLOCK_SIZE];
    file.read_exact_at(&mut after, 0)?;
    assert_eq!(after, before);
    volume.close()?;
    let mut reopened = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, byte) in [(0, 1), (320, 2), (640, 0)] {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [byte; BLOCK_SIZE]);
    }
    reopened.close()
}

#[test]
fn paged_flush_retains_dirty_mapping_and_recovers_without_a_checkpoint() -> io::Result<()> {
    let backing = fixture(320, 32)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    volume.write_block(7, &[0xac; BLOCK_SIZE])?;
    volume.flush()?;
    assert_eq!(volume.cache_snapshot().dirty_slots, 1);
    let checkpoint = checkpoint_load::load(&mut BackingIo::new(None)?, &backing.reopen()?, 32)?;
    assert_eq!(checkpoint.descriptor.generation, 0);
    assert_eq!(checkpoint.mapping_page_block.as_ref(), &[0]);
    drop(volume);
    let mut reopened = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let mut bytes = [0; BLOCK_SIZE];
    reopened.read_block(7, &mut bytes)?;
    assert_eq!(bytes, [0xac; BLOCK_SIZE]);
    reopened.close()
}
