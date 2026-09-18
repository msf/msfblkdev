use super::*;

#[test]
fn clean_checkpoint_maps_each_lba_to_latest_payload_and_checksum() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 2_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 8) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

    let mut volume = open(&backing.0)?;
    let volume_id = volume.mapping.checkpoint.descriptor.volume_id;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
    volume.write_block(0, &[0xc3; BLOCK_SIZE])?;
    volume.close()?;

    let file = File::open(&backing.0)?;
    let checkpoint = selected_checkpoint(&backing)?;
    let entries = snapshot_entries(&backing, checkpoint, 0)?;
    let expected = [
        (layout.log_start + 4, [0xc3; BLOCK_SIZE]),
        (layout.log_start + 2, [0x5a; BLOCK_SIZE]),
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
    Ok(())
}
struct CheckpointFallbackFixture {
    backing: TemporaryBacking,
    newest: CheckpointDescriptor,
    older_lsn: u64,
    last_lsn: u64,
    expected: [[u8; BLOCK_SIZE]; 3],
}

fn prepare_checkpoint_fallback_fixture() -> io::Result<CheckpointFallbackFixture> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 3_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let backing_bytes = u64::from(layout.log_start + 64) * BLOCK_SIZE as u64;
    backing.create_sized(backing_bytes)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

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
    let last_lsn = volume.mapping.tail.lsn;
    drop(volume);

    let file = OpenOptions::new().read(true).open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    let (green_bytes, blue_bytes) = descriptors.split_at_mut(BLOCK_SIZE);
    let green = decode_descriptor(green_bytes, CheckpointSlot::Green, backing_bytes)
        .expect("green checkpoint descriptor must be valid before corruption");
    let blue = decode_descriptor(blue_bytes, CheckpointSlot::Blue, backing_bytes)
        .expect("blue checkpoint descriptor must be valid before corruption");
    assert!(green.checkpoint_lsn > blue.checkpoint_lsn);
    assert_eq!(last_lsn, green.checkpoint_lsn + expected.len() as u64);

    for checkpoint in [green, blue] {
        let body_start = match checkpoint.slot {
            CheckpointSlot::Green => CheckpointSlot::Green.body_start(checkpoint.geometry()?),
            CheckpointSlot::Blue => CheckpointSlot::Blue.body_start(checkpoint.geometry()?),
        };
        let body_blocks = checkpoint.geometry()?.index_blocks;
        let mut body = vec![0; body_blocks as usize * BLOCK_SIZE];
        file.read_exact_at(&mut body, u64::from(body_start) * BLOCK_SIZE as u64)?;
        assert_eq!(xxh3_64(&body), checkpoint.index_checksum);
    }

    Ok(CheckpointFallbackFixture {
        backing,
        newest: green,
        older_lsn: blue.checkpoint_lsn,
        last_lsn,
        expected,
    })
}

fn verify_checkpoint_fallback(fixture: &CheckpointFallbackFixture) -> io::Result<()> {
    let opened = std::panic::catch_unwind(|| open(&fixture.backing.0))
        .map_err(|_| io::Error::other("public open panicked on checkpoint corruption"))?;
    let mut volume = opened?;
    assert_eq!(
        volume.mapping.checkpoint.descriptor.checkpoint_lsn,
        fixture.older_lsn
    );
    assert_eq!(volume.mapping.tail.lsn, fixture.last_lsn);
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
    let descriptor_offset = fixture.newest.slot as u64 * BLOCK_SIZE as u64 + 100;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.backing.0)?;
    let mut byte = [0; 1];
    file.read_exact_at(&mut byte, descriptor_offset)?;
    byte[0] ^= 0xff;
    file.write_all_at(&byte, descriptor_offset)?;
    file.sync_all()?;
    drop(file);

    verify_checkpoint_fallback(&fixture)
}

#[test]
fn falls_back_from_corrupted_newest_checkpoint_body_and_replays_tail() -> io::Result<()> {
    let fixture = prepare_checkpoint_fallback_fixture()?;
    let body_start = match fixture.newest.slot {
        CheckpointSlot::Green => CheckpointSlot::Green.body_start(fixture.newest.geometry()?),
        CheckpointSlot::Blue => CheckpointSlot::Blue.body_start(fixture.newest.geometry()?),
    };
    let body_offset = u64::from(body_start) * BLOCK_SIZE as u64 + 100;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.backing.0)?;
    let mut byte = [0; 1];
    file.read_exact_at(&mut byte, body_offset)?;
    byte[0] ^= 0xff;
    file.write_all_at(&byte, body_offset)?;
    file.sync_all()?;
    drop(file);

    verify_checkpoint_fallback(&fixture)
}

#[test]
fn rejects_two_unusable_checkpoint_roots_without_panic_or_mutation() -> io::Result<()> {
    let fixture = prepare_checkpoint_fallback_fixture()?;
    let newest_descriptor_offset = fixture.newest.slot as u64 * BLOCK_SIZE as u64 + 100;
    let older_body_start = match fixture.newest.slot {
        CheckpointSlot::Green => CheckpointSlot::Blue.body_start(fixture.newest.geometry()?),
        CheckpointSlot::Blue => CheckpointSlot::Green.body_start(fixture.newest.geometry()?),
    };
    let older_body_offset = u64::from(older_body_start) * BLOCK_SIZE as u64 + 100;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.backing.0)?;
    for offset in [newest_descriptor_offset, older_body_offset] {
        let mut byte = [0; 1];
        file.read_exact_at(&mut byte, offset)?;
        byte[0] ^= 0xff;
        file.write_all_at(&byte, offset)?;
    }
    file.sync_all()?;
    drop(file);

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
    let volume_blocks = 4_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let backing_blocks = u64::from(layout.log_start) + 64;
    let backing_bytes = backing_blocks * BLOCK_SIZE as u64;
    backing.create_sized(backing_bytes)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
    volume.close()?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    let (green_bytes, blue_bytes) = descriptors.split_at_mut(BLOCK_SIZE);
    let green = decode_descriptor(green_bytes, CheckpointSlot::Green, backing_bytes)
        .expect("green checkpoint descriptor must remain valid");
    let blue = decode_descriptor(blue_bytes, CheckpointSlot::Blue, backing_bytes)
        .expect("blue checkpoint descriptor must remain valid");
    let selected_checkpoint = [green, blue]
        .into_iter()
        .max_by_key(|checkpoint| checkpoint.checkpoint_lsn)
        .expect("formatted volume has checkpoint roots");
    assert_eq!(selected_checkpoint.checkpoint_lsn, 3);

    let checkpoint_footer = selected_checkpoint.last_record_boundary;
    write_raw_record_payloads(
        &file,
        selected_checkpoint.volume_id,
        4,
        checkpoint_footer,
        &[(0, 0xc3)],
    )?;
    let second_footer = checkpoint_footer + 2;
    write_raw_record_payloads(
        &file,
        selected_checkpoint.volume_id,
        5,
        second_footer,
        &[(2, 0x3c), (1, 0xe7), (3, 0x7e)],
    )?;
    let third_footer = second_footer + 4;
    write_raw_record_payloads(
        &file,
        selected_checkpoint.volume_id,
        6,
        third_footer,
        &[(2, 0xd4)],
    )?;
    file.sync_all()?;
    drop(file);

    let final_footer = third_footer + 2;
    let append_block = final_footer + 1;
    let replayed_blocks = u64::from(final_footer - checkpoint_footer);
    let expected = [
        (checkpoint_footer + 1, 0xc3),
        (second_footer + 2, 0xe7),
        (third_footer + 1, 0xd4),
        (second_footer + 3, 0x7e),
    ];

    let mut volume = open(&backing.0)?;
    assert_eq!(volume.mapping.tail.lsn, 6);
    assert_eq!(volume.mapping.tail.boundary, final_footer);

    assert_eq!(
        volume.mapping.checkpoint.descriptor.checkpoint_lsn,
        selected_checkpoint.checkpoint_lsn
    );
    assert_eq!(
        log_bytes_since_checkpoint(&volume),
        replayed_blocks * BLOCK_SIZE as u64
    );
    assert_eq!(volume.mapping.tail.boundary + 1, append_block);
    assert_eq!(
        volume.mapping.checkpoint.descriptor.slot.other(),
        match selected_checkpoint.slot {
            CheckpointSlot::Green => CheckpointSlot::Blue,
            CheckpointSlot::Blue => CheckpointSlot::Green,
        }
    );

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
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(
        u64::from(layout.log_start + MAXIMUM_RECORD_BLOCKS as u32) * BLOCK_SIZE as u64,
    )?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    file.write_all_at(
        &[0xa5; BLOCK_SIZE],
        u64::from(layout.log_start) * BLOCK_SIZE as u64,
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
    backing.create_sized(1024 * BLOCK_SIZE as u64)?;
    format(&backing.0, 320 * BLOCK_SIZE as u64)?;
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
    assert_eq!(volume.mapping.checkpoint.descriptor.checkpoint_lsn, 3);
    assert_eq!(volume.mapping.tail.lsn, 4);
    drop(volume);
    let mut reopened = open_with_options(&backing.0, options)?;
    let mut bytes = [0; BLOCK_SIZE];
    for lba in 0..320 {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [if lba < 160 { 3 } else { 0 }; BLOCK_SIZE]);
    }
    reopened.close()
}

#[test]
fn log_full_write_does_not_publish_live_checkpoint() -> io::Result<()> {
    const CHECKPOINT_BLOCKS: u64 = 340;

    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(
        u64::from(layout.log_start) * BLOCK_SIZE as u64 + CHECKPOINT_BLOCKS * BLOCK_SIZE as u64,
    )?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let options = VolumeOpenOptions {
        checkpoint_after_bytes: CHECKPOINT_BLOCKS * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    let mut volume = open_with_options(&backing.0, options)?;
    for lsn in 1..=(CHECKPOINT_BLOCKS - 2) / 2 {
        volume.write_block(0, &[lsn as u8; BLOCK_SIZE])?;
    }

    let state = (
        mapping_arrays(&volume)?.0.clone(),
        mapping_arrays(&volume)?.1.clone(),
        volume.mapping.tail.lsn,
        volume.mapping.checkpoint.descriptor.checkpoint_lsn,
        volume.mapping.tail.boundary,
        volume.mapping.checkpoint.descriptor.slot.other(),
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
            volume.mapping.tail.lsn,
            volume.mapping.checkpoint.descriptor.checkpoint_lsn,
            volume.mapping.tail.boundary,
            volume.mapping.checkpoint.descriptor.slot.other(),
            log_bytes_since_checkpoint(&volume),
        ),
        state
    );
    let mut unchanged_descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut unchanged_descriptors, 0)?;
    assert_eq!(unchanged_descriptors, descriptors);
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [((CHECKPOINT_BLOCKS - 2) / 2) as u8; BLOCK_SIZE]);
    Ok(())
}

#[test]
fn flush_persists_data_without_advancing_checkpoint() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(16 * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.flush()?;
    assert_eq!(selected_checkpoint(&backing)?.checkpoint_lsn, 0);
    drop(volume);
    let mut reopened = open(&backing.0)?;
    let mut bytes = [0; BLOCK_SIZE];
    reopened.read_block(0, &mut bytes)?;
    assert_eq!(bytes, [0xa5; BLOCK_SIZE]);
    reopened.close()
}

#[test]
fn close_persists_snapshot_index_and_descriptor() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(16 * BLOCK_SIZE as u64)?;
    format(&backing.0, 2 * BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(1, &[0xa5; BLOCK_SIZE])?;
    let expected = mapping_arrays(&volume)?;
    volume.close()?;
    let descriptor = selected_checkpoint(&backing)?;
    assert_eq!(descriptor.slot, CheckpointSlot::Blue);
    assert_eq!(descriptor.checkpoint_lsn, 2);
    assert_eq!(descriptor.last_record_boundary, 6);
    assert_eq!(descriptor.generation, 1);
    let entries = snapshot_entries(&backing, descriptor, 0)?;
    assert_eq!(entries.physical_addr[..2], expected.0);
    assert_eq!(entries.payload_checksum[..2], expected.1);
    let reopened = open(&backing.0)?;
    assert_eq!(reopened.mapping.checkpoint.descriptor, descriptor);
    assert_eq!(mapping_arrays(&reopened)?, expected);
    assert_eq!(reopened.mapping_stats().cache.dirty_slots, 0);
    reopened.close()
}

#[test]
fn close_without_mapping_changes_does_not_publish_checkpoint() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;

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
fn equal_generation_checkpoints_require_equivalent_mapping() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.close()?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut blue = [0; BLOCK_SIZE];
    file.read_exact_at(&mut blue, BLOCK_SIZE as u64)?;
    let mut body = [0; BLOCK_SIZE];
    file.read_exact_at(
        &mut body,
        u64::from(CheckpointSlot::Blue.body_start(layout)) * BLOCK_SIZE as u64,
    )?;
    file.write_all_at(
        &body,
        u64::from(CheckpointSlot::Green.body_start(layout)) * BLOCK_SIZE as u64,
    )?;
    let mut green = blue;
    green[5] = CheckpointSlot::Green as u8;
    green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
    let checksum = xxh3_64(&green);
    green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    file.write_all_at(&green, 0)?;

    let volume = open(&backing.0)?;
    assert_eq!(
        volume.mapping.checkpoint.descriptor.slot.other(),
        CheckpointSlot::Blue
    );
    drop(volume);

    green[CHECKPOINT_BODY_CHECKSUM_OFFSET] ^= 1;
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

fn decode_descriptor(
    bytes: &[u8],
    slot: CheckpointSlot,
    backing_bytes: u64,
) -> Option<CheckpointDescriptor> {
    CheckpointDescriptor::decode(bytes, slot, backing_bytes / BLOCK_SIZE as u64).ok()
}

fn selected_checkpoint(backing: &TemporaryBacking) -> io::Result<CheckpointDescriptor> {
    let file = backing.reopen()?;
    let (checkpoint, _) = crate::format_v3::checkpoint_load::load(
        &mut BackingIo::new(None)?,
        &file,
        file.metadata()?.len() / BLOCK_SIZE as u64,
    )?;
    Ok(checkpoint.descriptor)
}

fn snapshot_entries(
    backing: &TemporaryBacking,
    descriptor: CheckpointDescriptor,
    page_id: u32,
) -> io::Result<crate::mapping_cache::MappingEntries> {
    let file = backing.reopen()?;
    let mut index = AlignedBlock([0; BLOCK_SIZE]);
    let mut backing_io = BackingIo::new(None)?;
    let geometry = descriptor.geometry()?;
    read_block_exact(
        &mut backing_io,
        &file,
        descriptor.slot.body_start(geometry) + page_id / 1024,
        &mut index,
    )?;
    let boundary = read_u32(&index.0, (page_id as usize % 1024) * 4);
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(&mut backing_io, &file, boundary, &mut bytes)?;
    let record = crate::format_v3::mapping_page::MappingPageRecord::decode(
        &bytes.0,
        &crate::format_v3::mapping_page::PageExpectation {
            geometry,
            volume_id: descriptor.volume_id,
            page_id,
            boundary,
            maximum_lsn: descriptor.checkpoint_lsn,
        },
    )?;
    Ok(record.entries)
}
