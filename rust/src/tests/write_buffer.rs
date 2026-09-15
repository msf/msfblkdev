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

// Sizes a backing so the device clamp (log_capacity / 4) admits at least `payloads` buffer slots.
fn buffered_volume(payloads: u32) -> io::Result<(TemporaryBacking, VolumeOpenOptions, u32)> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = payloads + 8;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let log_blocks = 4 * u64::from(payloads) + 512;
    backing.create_sized(
        u64::from(layout.log_start) * BLOCK_SIZE as u64 + log_blocks * BLOCK_SIZE as u64,
    )?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
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
    for payloads in [1, 2, 337, 338, 339, 676] {
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
    // 340 payloads split into 338 + 2 across two records; footer overhead is tiny.
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
    volume.close()
}

#[test]
fn duplicate_lba_in_one_window_coalesces_to_the_latest_value() -> io::Result<()> {
    let (backing, options, _) = buffered_volume(8)?;
    let mut volume = open_with_options(&backing.0, options)?;

    let footer_before = volume.last_footer_block;
    for byte in [0xa5, 0x5a, 0xc3] {
        volume.write_block(0, &[byte; BLOCK_SIZE])?;
    }
    // The three same-address writes stayed coalesced in one slot; nothing reached the log yet.
    assert_eq!(volume.last_footer_block, footer_before);
    volume.flush()?;
    // One record with one payload was written (payload + footer = two blocks).
    assert_eq!(volume.last_footer_block, footer_before + 2);
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

/// A 64-block log, exactly the ublk/fio scenario 7 shape.
///
/// Capacity depends on how footers amortise, because the log is charged for what
/// a release really consumes. Write-through spends one payload plus one footer on
/// every write, so it fits 64/2 = 32. Batching spends one footer per record, so
/// the same log fits 60: three full 16-payload releases cost 17 blocks each, and
/// the remaining 13 blocks hold 12 payloads plus their footer.
#[test]
fn enospc_is_returned_by_the_offending_write_not_a_later_flush() -> io::Result<()> {
    for (write_buffer_bytes, capacity) in [(BLOCK_SIZE as u64, 32_u32), (4 * 1024 * 1024, 60)] {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 64_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let options = large_buffer_options(write_buffer_bytes);
        let mut volume = open_with_options(&backing.0, options)?;

        for lba in 0..capacity {
            volume.write_block(lba, &pattern(lba))?;
        }
        assert_eq!(
            volume
                .write_block(capacity, &[0xff; BLOCK_SIZE])
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC),
            "buffer {write_buffer_bytes} must reject write {capacity}"
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
fn checkpoint_body_round_trips_across_the_128kib_chunk_boundary() -> io::Result<()> {
    // 16384 blocks => 16 physical-map + 32 checksum-map = 48 body blocks per slot, spanning two
    // 128 KiB (32-block) chunks, so persist and load both cross a chunk boundary.
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 16_384_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    assert!(layout.physical_map_blocks + layout.checksum_map_blocks > 32);
    backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

    let writes = [(0_u32, 0xa5_u8), (1000, 0x5a), (16_383, 0xc3)];
    let mut volume = open(&backing.0)?;
    for (lba, byte) in writes {
        volume.write_block(lba, &[byte; BLOCK_SIZE])?;
    }
    volume.close()?;

    let mut volume = open(&backing.0)?;
    let mut actual = [0; BLOCK_SIZE];
    for (lba, byte) in writes {
        volume.read_block(lba, &mut actual)?;
        assert_eq!(
            actual, [byte; BLOCK_SIZE],
            "lba {lba} after chunked checkpoint"
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
    assert_eq!(volume.physical_blocks[0], 0);
    assert_eq!(volume.last_lsn, 0);
    assert_eq!(
        volume
            .write_block(0, &[0; BLOCK_SIZE])
            .unwrap_err()
            .to_string(),
        "volume failed"
    );
    Ok(())
}
