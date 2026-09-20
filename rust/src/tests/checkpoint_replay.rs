use super::*;
use crate::format_v4::checkpoint_load;

fn small_volume(volume_blocks: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, volume_blocks, 12, TINY_SEGMENT_BLOCKS)?;
    Ok(backing)
}

fn selected(backing: &TemporaryBacking) -> io::Result<checkpoint_load::Loaded> {
    let file = backing.reopen()?;
    checkpoint_load::load(
        &mut BackingIo::new(None)?,
        &file,
        file.metadata()?.len() / BLOCK_SIZE as u64,
        |geometry| geometry.segment_count,
    )
}

fn selected_checkpoint(backing: &TemporaryBacking) -> io::Result<CheckpointDescriptor> {
    Ok(selected(backing)?.descriptor)
}

fn snapshot_entries(
    backing: &TemporaryBacking,
    loaded: &checkpoint_load::Loaded,
    page_id: u32,
) -> io::Result<crate::mapping_cache::MappingEntries> {
    let file = backing.reopen()?;
    let boundary = loaded.log.index.snapshot_block(page_id);
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(&mut BackingIo::new(None)?, &file, boundary, &mut bytes)?;
    let record = crate::format_v4::mapping_page::MappingPageRecord::decode(
        &bytes.0,
        &crate::format_v4::mapping_page::PageExpectation {
            geometry: loaded.log.geometry,
            volume_id: loaded.log.volume_id,
            page_id,
            boundary,
            maximum_lsn: loaded.descriptor.checkpoint_lsn,
        },
    )?;
    Ok(record.entries)
}

fn corrupt_byte(backing: &TemporaryBacking, offset: u64) -> io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut byte = [0; 1];
    file.read_exact_at(&mut byte, offset)?;
    byte[0] ^= 0xff;
    file.write_all_at(&byte, offset)?;
    file.sync_all()
}

#[test]
fn clean_checkpoint_maps_each_lba_to_latest_payload_and_checksum() -> io::Result<()> {
    let backing = small_volume(2)?;
    let mut volume = open(&backing.0)?;
    let volume_id = volume.log.volume_id;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
    volume.write_block(0, &[0xc3; BLOCK_SIZE])?;
    volume.close()?;

    let file = File::open(&backing.0)?;
    let loaded = selected(&backing)?;
    let entries = snapshot_entries(&backing, &loaded, 0)?;
    // Three write-through records at 3, 5 and 7.
    let expected = [
        (FIRST_DATA_BLOCK + 4, [0xc3; BLOCK_SIZE]),
        (FIRST_DATA_BLOCK + 2, [0x5a; BLOCK_SIZE]),
    ];
    for (lba, (expected_physical_block, expected_payload)) in expected.iter().enumerate() {
        let physical_block = entries.physical_addr[lba];
        assert_eq!(physical_block, *expected_physical_block);

        let mut payload = [0; BLOCK_SIZE];
        file.read_exact_at(&mut payload, u64::from(physical_block) * BLOCK_SIZE as u64)?;
        assert_eq!(payload, *expected_payload);

        let mut checksum_input = [0; BLOCK_SIZE + 8];
        checksum_input[..4].copy_from_slice(&(lba as u32).to_le_bytes());
        checksum_input[4..8].copy_from_slice(&physical_block.to_le_bytes());
        checksum_input[8..].copy_from_slice(&payload);
        assert_eq!(
            entries.payload_checksum[lba],
            xxh3_64_with_seed(&checksum_input, volume_id)
        );
    }
    assert_eq!(loaded.live_blocks, 2);
    Ok(())
}

struct CheckpointFallbackFixture {
    backing: TemporaryBacking,
    newest: CheckpointDescriptor,
    older: CheckpointDescriptor,
    last_lsn: u64,
    expected: [[u8; BLOCK_SIZE]; 3],
}

fn prepare_checkpoint_fallback_fixture() -> io::Result<CheckpointFallbackFixture> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 3, 12, 64)?;

    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
    volume.close()?;

    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xc3; BLOCK_SIZE])?;
    volume.write_block(2, &[0x3c; BLOCK_SIZE])?;
    volume.close()?;

    let expected = [[0xd4; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x7e; BLOCK_SIZE]];
    let mut volume = open(&backing.0)?;
    for (lba, value) in expected.iter().enumerate() {
        volume.write_block(lba as u32, value)?;
    }
    volume.flush()?;
    let last_lsn = super::last_lsn(&volume);
    drop(volume);

    let file = OpenOptions::new().read(true).open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    let green = CheckpointDescriptor::decode(&descriptors[..BLOCK_SIZE], CheckpointSlot::Green)
        .expect("green checkpoint descriptor must be valid before corruption");
    let blue = CheckpointDescriptor::decode(&descriptors[BLOCK_SIZE..], CheckpointSlot::Blue)
        .expect("blue checkpoint descriptor must be valid before corruption");
    // Two closes published two roots; the format root has been replaced.
    assert!(green.checkpoint_lsn > blue.checkpoint_lsn);
    assert_eq!(last_lsn, green.checkpoint_lsn + expected.len() as u64);
    assert_ne!(green.direct_pages[0], 0);
    assert_ne!(blue.direct_pages[0], 0);

    Ok(CheckpointFallbackFixture {
        backing,
        newest: green,
        older: blue,
        last_lsn,
        expected,
    })
}

fn verify_checkpoint_fallback(fixture: &CheckpointFallbackFixture) -> io::Result<()> {
    let opened = std::panic::catch_unwind(|| open(&fixture.backing.0))
        .map_err(|_| io::Error::other("public open panicked on checkpoint corruption"))?;
    let mut volume = opened?;
    assert_eq!(volume.log.checkpoint_lsn(), fixture.older.checkpoint_lsn);
    assert_eq!(super::last_lsn(&volume), fixture.last_lsn);
    for (lba, expected) in fixture.expected.iter().enumerate() {
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(lba as u32, &mut actual)?;
        assert_eq!(&actual, expected);
    }
    volume.close()
}

#[test]
fn falls_back_from_corrupted_newest_descriptor_and_replays_tail() -> io::Result<()> {
    let fixture = prepare_checkpoint_fallback_fixture()?;
    corrupt_byte(
        &fixture.backing,
        fixture.newest.slot.block() as u64 * BLOCK_SIZE as u64 + 100,
    )?;
    verify_checkpoint_fallback(&fixture)
}

#[test]
fn falls_back_from_corrupted_newest_index_page_and_replays_tail() -> io::Result<()> {
    let fixture = prepare_checkpoint_fallback_fixture()?;
    corrupt_byte(
        &fixture.backing,
        u64::from(fixture.newest.direct_pages[0]) * BLOCK_SIZE as u64 + 100,
    )?;
    verify_checkpoint_fallback(&fixture)
}

#[test]
fn rejects_two_unusable_checkpoint_roots_without_panic_or_mutation() -> io::Result<()> {
    let fixture = prepare_checkpoint_fallback_fixture()?;
    corrupt_byte(
        &fixture.backing,
        fixture.newest.slot.block() as u64 * BLOCK_SIZE as u64 + 100,
    )?;
    corrupt_byte(
        &fixture.backing,
        u64::from(fixture.older.direct_pages[0]) * BLOCK_SIZE as u64 + 100,
    )?;

    let corrupted = std::fs::read(&fixture.backing.0)?;
    for _ in 0..2 {
        let opened = std::panic::catch_unwind(|| open(&fixture.backing.0))
            .map_err(|_| io::Error::other("public open panicked on checkpoint corruption"))?;
        let error = match opened {
            Ok(_) => return Err(io::Error::other("corrupted checkpoint roots were served")),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&fixture.backing.0)?, corrupted);
    }
    Ok(())
}

#[test]
fn reconstructs_every_recovery_cursor_from_replayed_state() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 4, 8, 64)?;

    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
    volume.close()?;

    let selected_checkpoint = selected_checkpoint(&backing)?;
    // Format table page, two data records, one snapshot and one direct page.
    assert_eq!(selected_checkpoint.checkpoint_lsn, 5);
    let checkpoint_footer = selected_checkpoint.head_boundary(HeadId::User);

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    write_raw_record_payloads(
        &file,
        selected_checkpoint.volume_id,
        6,
        checkpoint_footer,
        &[(0, 0xc3)],
    )?;
    let second_footer = checkpoint_footer + 2;
    write_raw_record_payloads(
        &file,
        selected_checkpoint.volume_id,
        7,
        second_footer,
        &[(2, 0x3c), (1, 0xe7), (3, 0x7e)],
    )?;
    let third_footer = second_footer + 4;
    write_raw_record_payloads(
        &file,
        selected_checkpoint.volume_id,
        8,
        third_footer,
        &[(2, 0xd4)],
    )?;
    file.sync_all()?;
    drop(file);

    let final_footer = third_footer + 2;
    let replayed_blocks = u64::from(final_footer - checkpoint_footer);
    let expected = [
        (checkpoint_footer + 1, 0xc3),
        (second_footer + 2, 0xe7),
        (third_footer + 1, 0xd4),
        (second_footer + 3, 0x7e),
    ];

    let mut volume = open(&backing.0)?;
    assert_eq!(last_lsn(&volume), 8);
    assert_eq!(
        user_position(&volume),
        LogPosition {
            lsn: 8,
            boundary: final_footer
        }
    );
    assert_eq!(
        volume.log.checkpoint_lsn(),
        selected_checkpoint.checkpoint_lsn
    );
    assert_eq!(
        log_bytes_since_checkpoint(&volume),
        replayed_blocks * BLOCK_SIZE as u64
    );
    assert_eq!(volume.log.head(HeadId::User).next_block, final_footer + 1);
    assert_eq!(volume.log.active_slot, selected_checkpoint.slot);
    assert_eq!(volume.space_usage()?.live_bytes, 4 * BLOCK_SIZE as u64);

    for (lba, &(physical_block, byte)) in expected.iter().enumerate() {
        let payload = [byte; BLOCK_SIZE];
        assert_eq!(mapping_arrays(&volume)?.0[lba], physical_block);
        assert_eq!(
            mapping_arrays(&volume)?.1[lba],
            payload_checksum(
                selected_checkpoint.volume_id,
                lba as u32,
                physical_block,
                &payload,
            )
        );
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(lba as u32, &mut actual)?;
        assert_eq!(actual, payload);
    }
    Ok(())
}

#[test]
fn rejects_invalid_checkpoint_after_bytes_before_touching_backing() -> io::Result<()> {
    const MINIMUM_CHECKPOINT_AFTER_BYTES: u64 = 339 * 4096;

    assert_eq!(
        MINIMUM_CHECKPOINT_AFTER_BYTES,
        MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64
    );
    assert_eq!(
        VolumeOpenOptions::default().checkpoint_after_bytes,
        64 * 1024 * 1024
    );

    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 1, 6, PROBE_SEGMENT_BLOCKS)?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    // Open would sanitize this block if validation happened too late.
    file.write_all_at(
        &[0xa5; BLOCK_SIZE],
        u64::from(FIRST_DATA_BLOCK) * BLOCK_SIZE as u64,
    )?;
    file.sync_all()?;
    drop(file);
    let original_bytes = std::fs::read(&backing.0)?;

    let missing = TemporaryBacking::new()?;
    let invalid_values = [
        0,
        MINIMUM_CHECKPOINT_AFTER_BYTES - BLOCK_SIZE as u64,
        MINIMUM_CHECKPOINT_AFTER_BYTES - BLOCK_SIZE as u64 + 1,
        MINIMUM_CHECKPOINT_AFTER_BYTES - 1,
        MINIMUM_CHECKPOINT_AFTER_BYTES + 1,
        VolumeOpenOptions::default().checkpoint_after_bytes - 1,
        VolumeOpenOptions::default().checkpoint_after_bytes + 1,
    ];
    for checkpoint_after_bytes in invalid_values {
        let options = VolumeOpenOptions {
            checkpoint_after_bytes,
            ..VolumeOpenOptions::default()
        };
        for path in [&backing.0, &missing.0] {
            let error = match open_with_options(path, options) {
                Ok(_) => return Err(io::Error::other("invalid replay bound was accepted")),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(std::fs::read(&backing.0)?, original_bytes);
        assert!(!missing.0.exists());
    }

    let minimum = VolumeOpenOptions {
        checkpoint_after_bytes: MINIMUM_CHECKPOINT_AFTER_BYTES,
        ..VolumeOpenOptions::default()
    };
    let volume = open_with_options(&backing.0, minimum)?;
    assert_eq!(
        volume.checkpoint_after_bytes,
        MINIMUM_CHECKPOINT_AFTER_BYTES
    );
    drop(volume);

    let defaults = VolumeOpenOptions::default();
    let volume = open_with_options(&backing.0, defaults)?;
    assert_eq!(volume.checkpoint_after_bytes, 64 * 1024 * 1024);
    Ok(())
}

#[test]
fn checkpoints_before_buffered_group_exceeding_replay_bound() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 336, 12, 256)?;
    let options = VolumeOpenOptions {
        checkpoint_after_bytes: 339 * BLOCK_SIZE as u64,
        write_buffer_bytes: 160 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    let mut volume = open_with_options(&backing.0, options)?;
    for round in 1..=3 {
        for lba in 0..160 {
            volume.write_block(lba, &[round; BLOCK_SIZE])?;
        }
        assert!(log_bytes_since_checkpoint(&volume) <= options.checkpoint_after_bytes);
        assert_eq!(volume.mapping_stats().io.page_writes, u64::from(round == 3));
    }
    assert!(volume.log.checkpoint_lsn() > FORMAT_LSN);
    assert!(last_lsn(&volume) > volume.log.checkpoint_lsn());
    drop(volume);
    let mut reopened = open_with_options(&backing.0, options)?;
    let mut bytes = [0; BLOCK_SIZE];
    for lba in 0..336 {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [if lba < 160 { 3 } else { 0 }; BLOCK_SIZE]);
    }
    reopened.close()
}

#[test]
fn log_full_write_does_not_publish_live_checkpoint() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 1, 5, TINY_SEGMENT_BLOCKS)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[1; BLOCK_SIZE])?;

    let state = (
        mapping_arrays(&volume)?.0.clone(),
        mapping_arrays(&volume)?.1.clone(),
        last_lsn(&volume),
        volume.log.checkpoint_lsn(),
        user_position(&volume),
        volume.log.active_slot,
        log_bytes_since_checkpoint(&volume),
    );
    let file = File::open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    assert_eq!(
        volume
            .write_block(0, &[0xff; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(
        (
            mapping_arrays(&volume)?.0.clone(),
            mapping_arrays(&volume)?.1.clone(),
            last_lsn(&volume),
            volume.log.checkpoint_lsn(),
            user_position(&volume),
            volume.log.active_slot,
            log_bytes_since_checkpoint(&volume),
        ),
        state
    );
    let mut unchanged_descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut unchanged_descriptors, 0)?;
    assert_eq!(unchanged_descriptors, descriptors);
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [1; BLOCK_SIZE]);
    Ok(())
}

#[test]
fn flush_persists_data_without_advancing_checkpoint() -> io::Result<()> {
    let backing = small_volume(1)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.flush()?;
    assert_eq!(selected_checkpoint(&backing)?.checkpoint_lsn, FORMAT_LSN);
    drop(volume);
    let mut reopened = open(&backing.0)?;
    let mut bytes = [0; BLOCK_SIZE];
    reopened.read_block(0, &mut bytes)?;
    assert_eq!(bytes, [0xa5; BLOCK_SIZE]);
    reopened.close()
}

#[test]
fn close_persists_snapshot_index_page_and_descriptor() -> io::Result<()> {
    let backing = small_volume(2)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(1, &[0xa5; BLOCK_SIZE])?;
    let expected = mapping_arrays(&volume)?;
    volume.close()?;
    let loaded = selected(&backing)?;
    let descriptor = &loaded.descriptor;
    assert_eq!(descriptor.slot, CheckpointSlot::Blue);
    // Table page 1, data 2, snapshot 3, direct page 4: the descriptor names the direct page.
    assert_eq!(descriptor.checkpoint_lsn, 4);
    assert_eq!(descriptor.head_boundary, [FIRST_DATA_BLOCK + 3, 0]);
    assert_eq!(descriptor.direct_pages[0], FIRST_DATA_BLOCK + 3);
    assert_eq!(descriptor.table_pages[0], 2);
    let entries = snapshot_entries(&backing, &loaded, 0)?;
    assert_eq!(entries.physical_addr[..2], expected.0);
    assert_eq!(entries.payload_checksum[..2], expected.1);
    let reopened = open(&backing.0)?;
    assert_eq!(reopened.log.checkpoint_lsn(), 4);
    assert_eq!(reopened.log.active_slot, CheckpointSlot::Blue);
    assert_eq!(mapping_arrays(&reopened)?, expected);
    assert_eq!(reopened.mapping_stats().cache.dirty_slots, 0);
    reopened.close()
}

#[test]
fn close_without_mapping_changes_does_not_publish_checkpoint() -> io::Result<()> {
    let backing = small_volume(1)?;

    let file = File::open(&backing.0)?;
    let mut before = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut before, 0)?;
    open(&backing.0)?.close()?;
    let mut after = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut after, 0)?;
    assert_eq!(after, before);

    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.close()?;
    file.read_exact_at(&mut before, 0)?;
    open(&backing.0)?.close()?;
    file.read_exact_at(&mut after, 0)?;
    assert_eq!(after, before);
    Ok(())
}

#[test]
fn equal_lsn_checkpoints_must_describe_one_checkpoint() -> io::Result<()> {
    let backing = small_volume(1)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.close()?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut blue = [0; BLOCK_SIZE];
    file.read_exact_at(&mut blue, BLOCK_SIZE as u64)?;
    let mut green = blue;
    green[5] = CheckpointSlot::Green as u8;
    green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
    let checksum = xxh3_64(&green);
    green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    file.write_all_at(&green, 0)?;

    let volume = open(&backing.0)?;
    assert_eq!(volume.log.active_slot, CheckpointSlot::Green);
    assert_eq!(volume.log.root_lsn[0], volume.log.root_lsn[1]);
    drop(volume);

    // The same LSN with a different table pointer is a conflict, not a fallback.
    green[352] ^= 1;
    green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
    let checksum = xxh3_64(&green);
    green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    file.write_all_at(&green, 0)?;
    assert_eq!(
        open(&backing.0).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}
