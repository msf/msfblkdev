use super::*;

#[test]
fn recovery_accepts_every_format_one_payload_count() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = MAXIMUM_PAYLOAD_BLOCKS as u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let record_blocks = (1..=MAXIMUM_PAYLOAD_BLOCKS as u64)
        .map(|payload_blocks| payload_blocks + 1)
        .sum::<u64>();
    let backing_blocks = u64::from(layout.log_start) + record_blocks + MAXIMUM_RECORD_BLOCKS;
    backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let volume_id = formatted_volume_id(&backing)?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut previous_footer_block = layout.log_start - 1;
    for payload_count in 1..=MAXIMUM_PAYLOAD_BLOCKS {
        let payloads = (0..payload_count)
            .map(|index| (index as u32, ((payload_count + index) % 251 + 1) as u8))
            .collect::<Vec<_>>();
        write_raw_record_payloads(
            &file,
            volume_id,
            payload_count as u64,
            previous_footer_block,
            &payloads,
        )?;
        previous_footer_block += payload_count as u32 + 1;
    }
    file.sync_all()?;
    drop(file);

    let mut volume = open(&backing.0)?;
    assert_eq!(volume.last_lsn, MAXIMUM_PAYLOAD_BLOCKS as u64);
    assert_eq!(volume.last_footer_block, previous_footer_block);
    assert_eq!(
        volume.log_bytes_since_checkpoint,
        record_blocks * BLOCK_SIZE as u64
    );
    for lba in 0..volume_blocks {
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(lba, &mut actual)?;
        let byte = ((MAXIMUM_PAYLOAD_BLOCKS + lba as usize) % 251 + 1) as u8;
        assert_eq!(actual, [byte; BLOCK_SIZE]);
    }
    Ok(())
}

#[test]
fn rejects_invalid_footer_with_zero_payloads() -> io::Result<()> {
    let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
    let clear_start = layout.log_start + 2;
    let backing_blocks = u64::from(clear_start) + MAXIMUM_RECORD_BLOCKS + 1;
    let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    file.write_all_at(
        &vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE],
        u64::from(clear_start) * BLOCK_SIZE as u64,
    )?;
    let mut footer = raw_footer(volume_id, 2, layout.log_start + 1, clear_start);
    assert_raw_footer_checksum_valid(&footer);
    let replay_cursor = Checkpoint {
        slot: CheckpointSlot::Green,
        volume_id,
        volume_blocks: 4,
        backing_blocks,
        checkpoint_lsn: 1,
        last_footer_block: layout.log_start + 1,
        layout,
        body_checksum: 0,
    };
    assert!(decode_write_tail_footer(&mut footer, replay_cursor, clear_start).is_none());
    file.write_all_at(&footer, u64::from(clear_start) * BLOCK_SIZE as u64)?;
    let preserved_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
    file.write_all_at(
        &[0x9d; BLOCK_SIZE],
        u64::from(preserved_block) * BLOCK_SIZE as u64,
    )?;
    file.sync_all()?;
    drop(file);

    verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    assert_zero_blocks(&file, clear_start, MAXIMUM_RECORD_BLOCKS as u32)?;
    let mut preserved = [0; BLOCK_SIZE];
    file.read_exact_at(
        &mut preserved,
        u64::from(preserved_block) * BLOCK_SIZE as u64,
    )?;
    assert_eq!(preserved, [0x9d; BLOCK_SIZE]);
    Ok(())
}

#[test]
fn rejects_invalid_footer_above_maximum_payloads() -> io::Result<()> {
    let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
    let clear_start = layout.log_start + 2;
    let invalid_footer_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
    let backing_blocks = u64::from(invalid_footer_block) + 1;
    let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    file.write_all_at(
        &vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE],
        u64::from(clear_start) * BLOCK_SIZE as u64,
    )?;
    let mut footer = raw_footer(volume_id, 2, layout.log_start + 1, invalid_footer_block);
    assert_raw_footer_checksum_valid(&footer);
    let replay_cursor = Checkpoint {
        slot: CheckpointSlot::Green,
        volume_id,
        volume_blocks: 4,
        backing_blocks,
        checkpoint_lsn: 1,
        last_footer_block: layout.log_start + 1,
        layout,
        body_checksum: 0,
    };
    assert!(decode_write_tail_footer(&mut footer, replay_cursor, invalid_footer_block).is_none());
    file.write_all_at(&footer, u64::from(invalid_footer_block) * BLOCK_SIZE as u64)?;
    file.sync_all()?;
    drop(file);

    verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    assert_zero_blocks(&file, clear_start, MAXIMUM_RECORD_BLOCKS as u32)?;
    let mut preserved_footer = [0; BLOCK_SIZE];
    file.read_exact_at(
        &mut preserved_footer,
        u64::from(invalid_footer_block) * BLOCK_SIZE as u64,
    )?;
    assert_eq!(preserved_footer, footer);
    Ok(())
}

#[test]
fn rejects_invalid_footer_declaring_position_beyond_backing() -> io::Result<()> {
    const BACKING_REMAINDER_BLOCKS: u32 = 5;

    let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
    let clear_start = layout.log_start + 2;
    let backing_blocks = u64::from(clear_start + BACKING_REMAINDER_BLOCKS);
    let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
    let actual_footer_block = backing_blocks as u32 - 1;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    file.write_all_at(
        &vec![0x7e; BACKING_REMAINDER_BLOCKS as usize * BLOCK_SIZE],
        u64::from(clear_start) * BLOCK_SIZE as u64,
    )?;
    let mut footer = raw_footer(volume_id, 2, layout.log_start + 1, backing_blocks as u32);
    for index in 0..(BACKING_REMAINDER_BLOCKS - 1) as usize {
        let lba_offset = FOOTER_LBA_OFFSET + index * size_of::<u32>();
        let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + index * size_of::<u64>();
        footer[lba_offset..lba_offset + size_of::<u32>()]
            .copy_from_slice(&(index as u32).to_le_bytes());
        let payload_block = clear_start + index as u32;
        let checksum =
            payload_checksum(volume_id, index as u32, payload_block, &[0x7e; BLOCK_SIZE]);
        footer[checksum_offset..checksum_offset + size_of::<u64>()]
            .copy_from_slice(&checksum.to_le_bytes());
    }
    seal_raw_footer(&mut footer);
    assert_raw_footer_checksum_valid(&footer);
    file.write_all_at(&footer, u64::from(actual_footer_block) * BLOCK_SIZE as u64)?;
    file.sync_all()?;
    drop(file);

    verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    assert_eq!(file.metadata()?.len(), backing_blocks * BLOCK_SIZE as u64);
    assert_zero_blocks(&file, clear_start, BACKING_REMAINDER_BLOCKS)
}

#[test]
fn rejects_invalid_footer_with_out_of_range_lba() -> io::Result<()> {
    verify_rejected_two_payload_footer(|footer, volume_id, first_payload_block| {
        footer[FOOTER_LBA_OFFSET..FOOTER_LBA_OFFSET + size_of::<u32>()]
            .copy_from_slice(&4_u32.to_le_bytes());
        let checksum = payload_checksum(volume_id, 4, first_payload_block, &[0x5a; BLOCK_SIZE]);
        footer[FOOTER_PAYLOAD_CHECKSUM_OFFSET..FOOTER_PAYLOAD_CHECKSUM_OFFSET + size_of::<u64>()]
            .copy_from_slice(&checksum.to_le_bytes());
    })
}

#[test]
fn rejects_invalid_footer_with_duplicate_lbas() -> io::Result<()> {
    verify_rejected_two_payload_footer(|footer, volume_id, first_payload_block| {
        let first_lba = footer[FOOTER_LBA_OFFSET..FOOTER_LBA_OFFSET + size_of::<u32>()].to_vec();
        footer[FOOTER_LBA_OFFSET + size_of::<u32>()..FOOTER_LBA_OFFSET + 2 * size_of::<u32>()]
            .copy_from_slice(&first_lba);
        let checksum = payload_checksum(volume_id, 1, first_payload_block + 1, &[0xc3; BLOCK_SIZE]);
        let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + size_of::<u64>();
        footer[checksum_offset..checksum_offset + size_of::<u64>()]
            .copy_from_slice(&checksum.to_le_bytes());
    })
}

#[test]
fn rejects_invalid_footer_with_nonzero_unused_lba() -> io::Result<()> {
    verify_rejected_two_payload_footer(|footer, _, _| {
        let unused_offset = FOOTER_LBA_OFFSET + 2 * size_of::<u32>();
        footer[unused_offset..unused_offset + size_of::<u32>()]
            .copy_from_slice(&3_u32.to_le_bytes());
    })
}

#[test]
fn rejects_invalid_footer_with_nonzero_unused_checksum() -> io::Result<()> {
    verify_rejected_two_payload_footer(|footer, _, _| {
        let unused_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + 2 * size_of::<u64>();
        footer[unused_offset..unused_offset + size_of::<u64>()]
            .copy_from_slice(&1_u64.to_le_bytes());
    })
}

fn recover_after_stale_tail_crash(test_name: &str, failpoint: &str) -> io::Result<()> {
    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let _volume = open(PathBuf::from(backing))?;
        return Err(io::Error::other("test failpoint did not pause child"));
    }

    let fixture = prepare_invalid_gap_fixture()?;
    sigkill_child_at_handshake(test_name, &fixture.backing.0, failpoint, Some(failpoint))?;
    if failpoint == STALE_TAIL_FSYNC_COMPLETE_FAILPOINT {
        verify_cleared_window(&fixture)?;
    }
    verify_invalid_gap_recovery(&fixture)
}

#[test]
fn recover_after_each_stale_tail_clear_block_boundary() -> io::Result<()> {
    let plan = configured_crash_test_plan(MAXIMUM_RECORD_BLOCKS)?;
    for block_index in plan.boundary_indices {
        let failpoint = format!("{STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT}-{block_index}");
        for _ in 0..plan.repetitions {
            recover_after_stale_tail_crash(
                "tests::stale_tail::recover_after_each_stale_tail_clear_block_boundary",
                &failpoint,
            )?;
        }
    }
    Ok(())
}

#[test]
fn recover_after_stale_tail_fsync_boundary() -> io::Result<()> {
    for _ in 0..configured_crash_test_plan(1)?.repetitions {
        recover_after_stale_tail_crash(
            "tests::stale_tail::recover_after_stale_tail_fsync_boundary",
            STALE_TAIL_FSYNC_COMPLETE_FAILPOINT,
        )?;
    }
    Ok(())
}

#[test]
fn invalid_gap_stops_replay_and_clears_only_bounded_window() -> io::Result<()> {
    let fixture = prepare_invalid_gap_fixture()?;
    verify_invalid_gap_recovery(&fixture)
}

#[test]
fn stale_tail_clear_stops_at_backing_eof() -> io::Result<()> {
    const EOF_REMAINDER_BLOCKS: u32 = 17;

    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    let clear_start = layout.log_start + 2;
    let backing_bytes = u64::from(clear_start + EOF_REMAINDER_BLOCKS) * BLOCK_SIZE as u64;
    backing.create_sized(backing_bytes)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let volume_id = formatted_volume_id(&backing)?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    write_raw_record(
        &file,
        volume_id,
        1,
        layout.log_start - 1,
        layout.log_start,
        0,
        0xa5,
    )?;
    file.write_all_at(
        &vec![0x7e; EOF_REMAINDER_BLOCKS as usize * BLOCK_SIZE],
        u64::from(clear_start) * BLOCK_SIZE as u64,
    )?;
    file.sync_all()?;
    drop(file);

    let volume = open(&backing.0)?;
    assert_eq!(volume.last_lsn, 1);
    drop(volume);
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    assert_eq!(file.metadata()?.len(), backing_bytes);
    for block in clear_start..clear_start + EOF_REMAINDER_BLOCKS {
        let mut bytes = [0xa5; BLOCK_SIZE];
        file.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
        assert_eq!(bytes, [0; BLOCK_SIZE]);
    }
    Ok(())
}
