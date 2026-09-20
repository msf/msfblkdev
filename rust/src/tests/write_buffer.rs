use super::*;

fn pattern(lba: u32) -> [u8; BLOCK_SIZE] {
    [(lba as u8).wrapping_mul(31).wrapping_add(7); BLOCK_SIZE]
}

fn large_buffer_options(write_buffer_bytes: u64) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes,
        ..VolumeOpenOptions::default()
    }
}

/// Eight 1024-block segments: the device clamp (capacity / 4 = 2048 slots) admits every
/// buffer size used here and two user segments remain above the reserve.
fn buffered_volume(payloads: u32) -> io::Result<(TemporaryBacking, VolumeOpenOptions, u32)> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = payloads + 8;
    format_segments(&backing, volume_blocks, 8, 1024)?;
    let options = large_buffer_options(u64::from(payloads) * BLOCK_SIZE as u64);
    Ok((backing, options, volume_blocks))
}

fn batch_boundary(payloads: u32) -> io::Result<()> {
    let (backing, options, _) = buffered_volume(payloads)?;
    let mut volume = open_with_options(&backing.0, options)?;
    for lba in 0..payloads {
        volume.write_block(lba, &pattern(lba))?;
    }
    volume.flush()?;
    volume.close()?;

    let mut volume = open_with_options(&backing.0, options)?;
    let mut actual = [0; BLOCK_SIZE];
    for lba in 0..payloads {
        volume.read_block(lba, &mut actual)?;
        assert_eq!(actual, pattern(lba), "payload {lba} of {payloads}");
    }
    volume.close()
}

#[test]
fn releases_at_record_boundaries_and_replays_multi_record_windows() -> io::Result<()> {
    for payloads in [1, 2, 335, 336, 337, 337, 338, 339, 676] {
        batch_boundary(payloads)?;
    }
    Ok(())
}

#[test]
fn release_stats_report_batch_occupancy_and_footer_ratio() -> io::Result<()> {
    let payloads = 340_u32;
    let (backing, options, _) = buffered_volume(payloads)?;
    let mut volume = open_with_options(&backing.0, options)?;
    for lba in 0..payloads {
        volume.write_block(lba, &pattern(lba))?;
    }
    volume.flush()?;
    let stats = volume.release_stats();
    // Packing splits 340 payloads into 338 + 2.
    assert_eq!(stats.payloads, u64::from(payloads));
    assert_eq!(stats.records, 2);
    assert_eq!(stats.payload_histogram[6], 1, "one 338-payload record");
    assert_eq!(stats.payload_histogram[1], 1, "one 2-payload record");
    let ratio = (stats.footer_bytes + stats.payload_bytes) as f64 / stats.payload_bytes as f64;
    assert!(
        ratio < 1.01,
        "batched footer ratio {ratio} should be near 1.0"
    );
    assert!(stats.full_releases >= 1);
    assert_eq!(volume.log.writes.user_payload_blocks, 340);
    assert_eq!(volume.log.writes.user_footer_blocks, 2);
    volume.close()
}

#[test]
fn duplicate_lba_in_one_window_coalesces_to_the_latest_value() -> io::Result<()> {
    let (backing, options, _) = buffered_volume(8)?;
    let mut volume = open_with_options(&backing.0, options)?;

    let footer_before = user_position(&volume).boundary;
    for byte in [0xa5, 0x5a, 0xc3] {
        volume.write_block(0, &[byte; BLOCK_SIZE])?;
    }
    // The three same-address writes stayed coalesced in one slot; nothing reached the log yet.
    assert_eq!(user_position(&volume).boundary, footer_before);
    volume.flush()?;
    // One record with one payload was written (payload + footer = two blocks).
    assert_eq!(user_position(&volume).boundary, footer_before + 2);
    volume.close()?;

    let mut volume = open_with_options(&backing.0, options)?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [0xc3; BLOCK_SIZE]);
    volume.close()
}

#[test]
fn read_after_write_without_flush_returns_the_buffered_value() -> io::Result<()> {
    let (backing, options, _) = buffered_volume(8)?;
    let mut volume = open_with_options(&backing.0, options)?;

    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(0, &[0x5a; BLOCK_SIZE])?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [0x5a; BLOCK_SIZE]);
    // A never-written neighbour still reads zero through the mapping table.
    volume.read_block(1, &mut actual)?;
    assert_eq!(actual, [0; BLOCK_SIZE]);
    volume.close()
}

/// Eight tiny segments: two heads, three reserve segments and three user segments.
///
/// Admission reserves payloads, footers, existing and pending snapshots and one footer per
/// possible crossing. The offending write, not a later flush, sees `ENOSPC`, and every
/// admitted write survives a reopen.
#[test]
fn enospc_is_returned_by_the_offending_write_not_a_later_flush() -> io::Result<()> {
    for write_buffer_bytes in [BLOCK_SIZE as u64, 4 * 1024 * 1024] {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 64_u32;
        format_segments(&backing, volume_blocks, 8, TINY_SEGMENT_BLOCKS)?;
        let options = large_buffer_options(write_buffer_bytes);
        let mut volume = open_with_options(&backing.0, options)?;

        let mut capacity = 0;
        // Bounded by the volume: every LBA is written at most once.
        while capacity < volume_blocks {
            match volume.write_block(capacity, &pattern(capacity)) {
                Ok(()) => capacity += 1,
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                    break;
                }
            }
        }
        assert!(
            capacity >= 6,
            "buffer {write_buffer_bytes} admitted only {capacity}"
        );
        assert!(
            capacity < volume_blocks,
            "buffer {write_buffer_bytes} never filled the log"
        );
        // Admission never dips into the reserve; a buffered group may stop one segment above it.
        let usage = volume.space_usage()?;
        assert_eq!(usage.reserve_segments, 3);
        assert!(
            usage.free_segments >= 3 && usage.free_segments <= 4,
            "{usage:?}"
        );
        volume.flush()?;
        volume.close()?;

        let mut volume = open_with_options(&backing.0, options)?;
        let mut actual = [0; BLOCK_SIZE];
        for lba in 0..capacity {
            volume.read_block(lba, &mut actual)?;
            assert_eq!(actual, pattern(lba), "surviving write {lba}");
        }
        volume.read_block(capacity, &mut actual)?;
        assert_eq!(actual, [0; BLOCK_SIZE], "rejected write never persisted");
        volume.close()?;
    }
    Ok(())
}

#[test]
fn checkpoint_index_round_trips_across_direct_pages() -> io::Result<()> {
    // More than 1008 mapping pages requires a second direct index page.
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 336 * 1008 + 1;
    format_segments(&backing, volume_blocks, 8, 64)?;

    let writes = [(0_u32, 0xa5_u8), (1000, 0x5a), (volume_blocks - 1, 0xc3)];
    let mut volume = open(&backing.0)?;
    assert_eq!(volume.log.geometry.direct_pages, 2);
    for (lba, byte) in writes {
        volume.write_block(lba, &[byte; BLOCK_SIZE])?;
    }
    volume.close()?;

    let mut volume = open(&backing.0)?;
    assert_ne!(volume.log.index.direct_block(0), 0);
    assert_ne!(volume.log.index.direct_block(1), 0);
    let mut actual = [0; BLOCK_SIZE];
    for (lba, byte) in writes {
        volume.read_block(lba, &mut actual)?;
        assert_eq!(
            actual, [byte; BLOCK_SIZE],
            "lba {lba} after tree publication"
        );
    }
    volume.read_block(500, &mut actual)?;
    assert_eq!(actual, [0; BLOCK_SIZE]);
    volume.close()
}

#[test]
fn short_backing_write_fails_the_release_without_publishing() -> io::Result<()> {
    let (backing, options, _) = buffered_volume(8)?;
    let mut volume = open_with_options(&backing.0, options)?;
    // Force the next backing writev to fail by swapping in a read-only descriptor.
    volume.backing = backing.reopen()?;

    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    let error = volume.flush().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    assert_eq!(mapping_arrays(&volume)?.0[0], 0);
    assert_eq!(last_lsn(&volume), FORMAT_LSN);
    assert_eq!(
        volume
            .write_block(0, &[0; BLOCK_SIZE])
            .unwrap_err()
            .to_string(),
        "volume failed"
    );
    Ok(())
}
