use super::*;
use crate::format_v3::{
    checkpoint_publish::LogPosition,
    tail::{self, LogRecord},
};
use crate::io_accounting::{Reader, Writer};
use crate::mapping_cache::sizing::Sizing;

fn fixture(payloads: u32, pages: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized((u64::from(payloads) * 4 + 1024) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(pages) * 336 * BLOCK_SIZE as u64)?;
    Ok(backing)
}

#[test]
fn sorting_buffer_preserves_payload_locations_and_coalesced_values_without_allocating() {
    let mut buffer = WriteBackBuffer::with_capacity(4096);
    for lba in (0..4096).rev() {
        buffer.store(lba, &[lba as u8; BLOCK_SIZE]);
    }
    buffer.store(336, &[99; BLOCK_SIZE]);
    let (_, allocations) = super::allocations::measure(|| buffer.sort_by_lba());
    assert_eq!(allocations, 0);
    assert!(buffer.lbas.windows(2).all(|pair| pair[0] < pair[1]));
    for lba in 0..4096 {
        let byte = if lba == 336 { 99 } else { lba as u8 };
        assert_eq!(buffer.get(lba), Some(&[byte; BLOCK_SIZE]));
    }
    buffer.clear();
    assert!(buffer.store(7, &[42; BLOCK_SIZE]));
    assert_eq!(buffer.get(7), Some(&[42; BLOCK_SIZE]));
}

#[test]
fn packed_buffers_cross_record_boundaries_with_one_log_flush() -> io::Result<()> {
    for payloads in [338, 339, 1024, 1500] {
        let backing = fixture(payloads, payloads)?;
        let ledger = TemporaryBacking::new()?;
        // SAFETY: the private ledger outlives its handles and is never externally modified.
        let reader = unsafe { Reader::create(&ledger.0, 128 << 20)? };
        // SAFETY: same private inode lifetime as the reader above.
        let writer = unsafe { Writer::open(&ledger.0)? };
        let options = VolumeOpenOptions {
            write_buffer_bytes: u64::from(payloads + 1) * BLOCK_SIZE as u64,
            ..VolumeOpenOptions::default()
        };
        let mut volume = open_with_accounting(&backing.0, options, writer)?;
        let before = reader.snapshot()?;
        for page in (0..payloads).rev() {
            volume.write_block(page * 336, &[page as u8; BLOCK_SIZE])?;
        }
        volume.flush()?;
        let after = reader.snapshot()?;
        let records = u64::from(payloads).div_ceil(MAXIMUM_PAYLOAD_BLOCKS as u64);
        assert_eq!(
            after.write_completed_operations - before.write_completed_operations,
            records
        );
        assert_eq!(
            after.flush_completed_operations - before.flush_completed_operations,
            1
        );
        assert_eq!(
            after.write_completed_bytes - before.write_completed_bytes,
            (u64::from(payloads) + records) * BLOCK_SIZE as u64
        );
        assert_eq!(volume.release_stats().records, records);
        assert_eq!(volume.mapping_stats().pressure_checkpoints, 0);
        assert_eq!(volume.mapping_stats().cache.dirty_slots, payloads as usize);
        check_sorted_records(&volume, payloads)?;
        drop(volume);
        let mut reopened = open_with_options(&backing.0, options)?;
        let mut bytes = [0; BLOCK_SIZE];
        for page in 0..payloads {
            reopened.read_block(page * 336, &mut bytes)?;
            assert_eq!(bytes, [page as u8; BLOCK_SIZE]);
        }
        reopened.close()?;
    }
    Ok(())
}

fn check_sorted_records(volume: &Volume, payloads: u32) -> io::Result<()> {
    let descriptor = volume.mapping.checkpoint.descriptor;
    let mut position = LogPosition {
        lsn: descriptor.checkpoint_lsn,
        boundary: descriptor.last_record_boundary,
    };
    let mut io = BackingIo::new(None)?;
    let mut seen = 0;
    while seen < payloads {
        let LogRecord::Data(record) =
            tail::read_next(&mut io, &volume.backing, descriptor, position)?.unwrap()
        else {
            panic!("no mapping snapshots inside a packed release");
        };
        let count = record.header.payload_count()? as u32;
        assert_eq!(count, (payloads - seen).min(MAXIMUM_PAYLOAD_BLOCKS as u32));
        for (index, lba) in record.lba_ids[..count as usize].iter().enumerate() {
            assert_eq!(*lba, (seen + index as u32) * 336);
        }
        seen += count;
        position = LogPosition {
            lsn: record.header.lsn,
            boundary: record.header.boundary,
        };
    }
    assert_eq!(position, volume.mapping.tail);
    Ok(())
}

#[test]
fn incompatible_cache_or_checkpoint_budget_is_rejected_before_recovery_writes() -> io::Result<()> {
    let backing = fixture(1024, 1024)?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    // Open would sanitize this tail if configuration validation happened too late.
    file.write_all_at(&[0x5a; BLOCK_SIZE], 16 * BLOCK_SIZE as u64)?;
    let before = std::fs::read(&backing.0)?;
    let minimum = Sizing::for_capacity(1024).unwrap().allocated_bytes as u64;
    for options in [
        VolumeOpenOptions {
            write_buffer_bytes: 4 << 20,
            cache_budget_bytes: Some(minimum - 1),
            ..VolumeOpenOptions::default()
        },
        VolumeOpenOptions {
            write_buffer_bytes: 4 << 20,
            checkpoint_after_bytes: 339 * BLOCK_SIZE as u64,
            ..VolumeOpenOptions::default()
        },
    ] {
        let error = open_with_options(&backing.0, options).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(&backing.0)?, before);
    }
    let options = VolumeOpenOptions {
        write_buffer_bytes: 4 << 20,
        cache_budget_bytes: Some(minimum),
        ..VolumeOpenOptions::default()
    };
    let volume = open_with_options(&backing.0, options)?;
    assert_eq!(volume.mapping_stats().cache.sizing.capacity, 1024);
    volume.close()
}

#[test]
fn pressure_is_resolved_before_a_group_and_overlapping_dirty_pages_need_no_checkpoint()
-> io::Result<()> {
    let backing = fixture(16, 4)?;
    let options = VolumeOpenOptions {
        write_buffer_bytes: 2 * BLOCK_SIZE as u64,
        cache_budget_bytes: Some(Sizing::for_capacity(2).unwrap().allocated_bytes as u64),
        ..VolumeOpenOptions::default()
    };
    let mut volume = open_with_options(&backing.0, options)?;
    for lba in [336, 0, 0, 336] {
        volume.write_block(lba, &[1; BLOCK_SIZE])?;
    }
    assert_eq!(volume.mapping_stats().pressure_checkpoints, 0);
    volume.write_block(1008, &[2; BLOCK_SIZE])?;
    volume.write_block(672, &[3; BLOCK_SIZE])?;
    assert_eq!(volume.mapping_stats().pressure_checkpoints, 1);
    assert_eq!(volume.release_stats().records, 3);
    volume.flush()?;
    drop(volume);
    let mut reopened = open_with_options(&backing.0, options)?;
    for (lba, byte) in [(0, 1), (336, 1), (672, 3), (1008, 2)] {
        let mut bytes = [0; BLOCK_SIZE];
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [byte; BLOCK_SIZE]);
    }
    reopened.close()
}

#[test]
fn packed_record_recovers_all_mappings_after_a_partial_in_memory_update() -> io::Result<()> {
    let options = VolumeOpenOptions {
        write_buffer_bytes: 1024 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open_with_options(PathBuf::from(path), options)?;
        for page in (0..700).rev() {
            volume.write_block(page * 336, &[(page % 251 + 1) as u8; BLOCK_SIZE])?;
        }
        volume.flush()?;
        return Err(io::Error::other("packed release failpoint did not pause"));
    }
    for boundary in [
        RECORD_WRITE_COMPLETE_FAILPOINT,
        MAPPING_PUBLISHED_FAILPOINT,
        LOG_FSYNC_COMPLETE_FAILPOINT,
    ] {
        for _ in 0..configured_crash_test_plan(3)?.repetitions {
            let backing = fixture(1024, 700)?;
            sigkill_child_at_handshake(
                "tests::packed_writes::packed_record_recovers_all_mappings_after_a_partial_in_memory_update",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            let mut reopened = open_with_options(&backing.0, options)?;
            for page in 0..700 {
                let mut bytes = [0; BLOCK_SIZE];
                reopened.read_block(page * 336, &mut bytes)?;
                let byte = if boundary == LOG_FSYNC_COMPLETE_FAILPOINT || page < 338 {
                    (page % 251 + 1) as u8
                } else {
                    0
                };
                assert_eq!(bytes, [byte; BLOCK_SIZE], "{boundary}, page {page}");
            }
            reopened.close()?;
        }
    }
    Ok(())
}

#[test]
fn preflight_checkpoint_crashes_preserve_the_previous_complete_group() -> io::Result<()> {
    let options = VolumeOpenOptions {
        write_buffer_bytes: 2 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open_with_options(PathBuf::from(path), options)?;
        volume.write_block(672, &[2; BLOCK_SIZE])?;
        volume.write_block(1008, &[3; BLOCK_SIZE])?;
        return Err(io::Error::other(
            "pressure checkpoint failpoint did not pause",
        ));
    }
    let boundaries = [
        "mapping-page-write-before-0",
        "mapping-page-write-complete-0",
        "checkpoint-body-block-complete-0",
        CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT,
        DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
        DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
    ];
    for boundary in boundaries {
        for _ in 0..configured_crash_test_plan(boundaries.len() as u64)?.repetitions {
            let backing = fixture(16, 4)?;
            let mut volume = open_with_options(&backing.0, options)?;
            for lba in [0, 336] {
                volume.write_block(lba, &[1; BLOCK_SIZE])?;
            }
            volume.flush()?;
            drop(volume);
            sigkill_child_at_handshake(
                "tests::packed_writes::preflight_checkpoint_crashes_preserve_the_previous_complete_group",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            let mut reopened = open_with_options(&backing.0, options)?;
            for lba in [0, 336, 672, 1008] {
                let mut bytes = [0; BLOCK_SIZE];
                reopened.read_block(lba, &mut bytes)?;
                assert_eq!(bytes, [u8::from(lba < 672); BLOCK_SIZE]);
            }
            reopened.close()?;
        }
    }
    Ok(())
}
