use super::*;

#[test]
fn multiple_lbas_read_correctly_before_and_after_reopen() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 2_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

    let expected = [[0xa5; BLOCK_SIZE], [0x5a; BLOCK_SIZE]];
    let mut actual = [0; BLOCK_SIZE];
    let mut volume = open(&backing.0)?;
    for (lba, block) in expected.iter().enumerate() {
        volume.write_block(lba as u32, block)?;
    }
    for (lba, block) in expected.iter().enumerate() {
        volume.read_block(lba as u32, &mut actual)?;
        assert_eq!(&actual, block);
    }
    volume.close()?;

    let mut volume = open(&backing.0)?;
    for (lba, block) in expected.iter().enumerate() {
        volume.read_block(lba as u32, &mut actual)?;
        assert_eq!(&actual, block);
    }
    volume.close()
}

#[test]
fn repeated_overwrite_returns_only_latest_value() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;

    let values = [[0xa5; BLOCK_SIZE], [0x5a; BLOCK_SIZE], [0xc3; BLOCK_SIZE]];
    for expected in values {
        volume.write_block(0, &expected)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, expected);
    }
    volume.flush()?;
    volume.close()?;

    let mut volume = open(&backing.0)?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, values[2]);
    volume.close()
}

#[test]
fn overwriting_lba_with_zeroes_returns_zeroes_before_and_after_reopen() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;

    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(0, &[0; BLOCK_SIZE])?;
    let mut actual = [0xa5; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [0; BLOCK_SIZE]);
    volume.close()?;

    let mut volume = open(&backing.0)?;
    actual.fill(0xa5);
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [0; BLOCK_SIZE]);
    volume.close()
}

#[test]
fn raw_footer_linkage_and_lsns_are_contiguous() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    let initial_footer_block = volume.last_footer_block;

    for byte in [0xa5, 0x5a, 0xc3] {
        volume.write_block(0, &[byte; BLOCK_SIZE])?;
    }

    let file = File::open(&backing.0)?;
    let mut previous_footer_block = initial_footer_block;
    for lsn in 1..=3 {
        let footer_block = previous_footer_block + 2;
        let mut footer = [0; BLOCK_SIZE];
        file.read_exact_at(&mut footer, u64::from(footer_block) * BLOCK_SIZE as u64)?;
        assert_eq!(read_u64(&footer, 16), lsn);
        assert_eq!(read_u32(&footer, 24), previous_footer_block);
        assert_eq!(read_u32(&footer, 28), footer_block);
        previous_footer_block = footer_block;
    }
    Ok(())
}
#[test]
fn log_full_write_leaves_last_successful_value_readable_after_reopen() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;

    let expected = [0xa5; BLOCK_SIZE];
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &expected)?;
    let physical_blocks = volume.physical_blocks.clone();
    let checksums = volume.checksums.clone();
    let cursors = (
        volume.last_lsn,
        volume.durable_lsn,
        volume.checkpoint_lsn,
        volume.last_footer_block,
        volume.log_bytes_since_checkpoint,
        volume.next_checkpoint_slot,
        volume.failed,
    );
    let file = File::open(&backing.0)?;
    let mut log_bytes = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(
        &mut log_bytes,
        u64::from(layout.log_start) * BLOCK_SIZE as u64,
    )?;

    assert_eq!(
        volume
            .write_block(0, &[0x5a; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );

    assert_eq!(volume.physical_blocks, physical_blocks);
    assert_eq!(volume.checksums, checksums);
    assert_eq!(
        (
            volume.last_lsn,
            volume.durable_lsn,
            volume.checkpoint_lsn,
            volume.last_footer_block,
            volume.log_bytes_since_checkpoint,
            volume.next_checkpoint_slot,
            volume.failed,
        ),
        cursors
    );
    let mut unchanged_log_bytes = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(
        &mut unchanged_log_bytes,
        u64::from(layout.log_start) * BLOCK_SIZE as u64,
    )?;
    assert_eq!(unchanged_log_bytes, log_bytes);

    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    volume.close()?;

    let mut volume = open(&backing.0)?;
    actual.fill(0);
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    volume.close()
}

#[test]
fn out_of_range_write_leaves_mapping_and_cursors_unchanged() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;

    let expected = [0xa5; BLOCK_SIZE];
    volume.write_block(0, &expected)?;
    volume.flush()?;
    let physical_blocks = volume.physical_blocks.clone();
    let checksums = volume.checksums.clone();
    let cursors = (
        volume.last_lsn,
        volume.durable_lsn,
        volume.checkpoint_lsn,
        volume.last_footer_block,
        volume.next_checkpoint_slot,
        volume.log_bytes_since_checkpoint,
        volume.failed,
    );

    let error = volume
        .write_block(volume.volume_blocks, &[0x5a; BLOCK_SIZE])
        .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(volume.physical_blocks, physical_blocks);
    assert_eq!(volume.checksums, checksums);
    assert_eq!(
        (
            volume.last_lsn,
            volume.durable_lsn,
            volume.checkpoint_lsn,
            volume.last_footer_block,
            volume.next_checkpoint_slot,
            volume.log_bytes_since_checkpoint,
            volume.failed,
        ),
        cursors
    );
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn write_block_appends_complete_raw_record_and_publishes_mapping() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 2_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;

    let lba = 1_u32;
    let payload = [0xa5; BLOCK_SIZE];
    let previous_footer_block = volume.last_footer_block;
    let payload_block = previous_footer_block + 1;
    let footer_block = payload_block + 1;
    volume.write_block(lba, &payload)?;

    let mut record = [0; 2 * BLOCK_SIZE];
    File::open(&backing.0)?
        .read_exact_at(&mut record, u64::from(payload_block) * BLOCK_SIZE as u64)?;
    assert_eq!(&record[..BLOCK_SIZE], &payload);
    let footer = &mut record[BLOCK_SIZE..];
    assert_eq!(&footer[..4], FOOTER_MAGIC);
    assert_eq!(&footer[4..8], &[FORMAT_VERSION, RECORD_KIND_WRITE, 0, 0]);
    assert_eq!(read_u64(footer, 8), volume.volume_id);
    assert_eq!(read_u64(footer, 16), 1);
    assert_eq!(read_u32(footer, 24), previous_footer_block);
    assert_eq!(read_u32(footer, 28), footer_block);
    assert_eq!(read_u32(footer, FOOTER_LBA_OFFSET), lba);
    let mut checksum_input = [0; BLOCK_SIZE + 8];
    checksum_input[..4].copy_from_slice(&lba.to_le_bytes());
    checksum_input[4..8].copy_from_slice(&payload_block.to_le_bytes());
    checksum_input[8..].copy_from_slice(&payload);
    let expected_payload_checksum = xxh3_64_with_seed(&checksum_input, volume.volume_id);
    assert_eq!(
        read_u64(footer, FOOTER_PAYLOAD_CHECKSUM_OFFSET),
        expected_payload_checksum
    );
    assert!(
        footer[FOOTER_LBA_OFFSET + 4..FOOTER_PAYLOAD_CHECKSUM_OFFSET]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(
        footer[FOOTER_PAYLOAD_CHECKSUM_OFFSET + 8..FOOTER_CHECKSUM_OFFSET]
            .iter()
            .all(|byte| *byte == 0)
    );
    let stored_footer_checksum = read_u64(footer, FOOTER_CHECKSUM_OFFSET);
    footer[FOOTER_CHECKSUM_OFFSET..].fill(0);
    assert_eq!(xxh3_64(footer), stored_footer_checksum);

    assert_eq!(volume.physical_blocks, [0, payload_block]);
    assert_eq!(volume.checksums, [0, expected_payload_checksum]);
    assert_eq!(volume.last_lsn, 1);
    assert_eq!(volume.durable_lsn, 0);
    assert_eq!(volume.last_footer_block, footer_block);
    assert_eq!(volume.log_bytes_since_checkpoint, (2 * BLOCK_SIZE) as u64);
    Ok(())
}

#[test]
fn failed_write_is_not_published() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.backing = backing.reopen()?;

    let initial_footer_block = volume.last_footer_block;
    let error = volume.write_block(0, &[0xa5; BLOCK_SIZE]).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    assert_eq!(volume.physical_blocks, [0]);
    assert_eq!(volume.checksums, [0]);
    assert_eq!(volume.last_lsn, 0);
    assert_eq!(volume.durable_lsn, 0);
    assert_eq!(volume.last_footer_block, initial_footer_block);
    assert_eq!(volume.log_bytes_since_checkpoint, 0);

    let mut block = [0x5a; BLOCK_SIZE];
    assert_eq!(
        volume.read_block(0, &mut block).unwrap_err().to_string(),
        "volume failed"
    );
    assert_eq!(block, [0x5a; BLOCK_SIZE]);
    assert_eq!(
        volume
            .write_block(0, &[0; BLOCK_SIZE])
            .unwrap_err()
            .to_string(),
        "volume failed"
    );
    assert_eq!(volume.flush().unwrap_err().to_string(), "volume failed");

    assert_eq!(volume.close().unwrap_err().to_string(), "volume failed");
    Ok(())
}

#[test]
fn public_operations_complete_v0_4_acceptance() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 2_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

    let mut expected = [0; BLOCK_SIZE];
    for (index, byte) in expected.iter_mut().enumerate() {
        *byte = index as u8;
    }
    {
        let mut volume = open(&backing.0)?;
        let mut actual = [0xa5; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0; BLOCK_SIZE]);
        assert_eq!(
            volume
                .read_block(volume_blocks, &mut actual)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        volume.write_block(1, &expected)?;
        volume.read_block(1, &mut actual)?;
        assert_eq!(actual, expected);
        volume.flush()?;
        volume.close()?;
    }
    {
        let mut volume = open(&backing.0)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(1, &mut actual)?;
        assert_eq!(actual, expected);
        volume.close()?;
    }

    let file = OpenOptions::new().write(true).open(&backing.0)?;
    file.write_all_at(
        &[expected[0] ^ 0xff],
        u64::from(layout.log_start) * BLOCK_SIZE as u64,
    )?;
    file.sync_all()?;
    let mut volume = open(&backing.0)?;
    let mut actual = [0x5a; BLOCK_SIZE];
    assert_eq!(
        volume.read_block(1, &mut actual).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(actual, [0x5a; BLOCK_SIZE]);
    Ok(())
}
