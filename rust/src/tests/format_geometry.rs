use super::*;

#[test]
fn public_cache_configuration_reports_requested_and_effective_capacity() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let logical_blocks = 673_u32;
    format_segments(&backing, logical_blocks, 8, TINY_SEGMENT_BLOCKS)?;
    // Without an explicit budget the cache takes the remaining heap, capped at three pages.
    for (budget, capacity, hits, misses) in
        [(None, 3, 2, 3), (Some(1), 1, 1, 4), (Some(20_000), 3, 2, 3)]
    {
        let mut volume = open_with_options(
            &backing.0,
            VolumeOpenOptions {
                cache_budget_bytes: budget,
                ..VolumeOpenOptions::default()
            },
        )?;
        let initial = volume.mapping_stats();
        if let Some(budget) = budget {
            assert_eq!(initial.cache.sizing.requested_bytes, budget);
        } else {
            assert!(
                initial.cache.sizing.requested_bytes >= initial.cache.sizing.allocated_bytes as u64
            );
        }
        assert_eq!(initial.cache.sizing.capacity, capacity);
        // Three snapshot pointers, one direct-page pointer and one dirty flag.
        assert_eq!(initial.index_bytes, 3 * 4 + 4 + 1);
        assert!(initial.cache.sizing.allocated_bytes >= capacity * 4032);
        assert_eq!(initial.cache.counters.foreground.hit_rate(), None);
        let budget_report = volume.heap_budget();
        assert!(budget_report.allocated_bytes() <= budget_report.total_bytes);
        assert_eq!(budget_report.total_bytes, MINIMUM_HEAP_BYTES);
        let mut bytes = [0xff; BLOCK_SIZE];
        for lba in [0, 0, 336, 672, 0] {
            volume.read_block(lba, &mut bytes)?;
            assert_eq!(bytes, [0; BLOCK_SIZE]);
        }
        let stats = volume.mapping_stats();
        assert_eq!(stats.cache.counters.foreground.hits, hits);
        assert_eq!(stats.cache.counters.foreground.misses, misses);
        assert_eq!(
            stats.io.page_reads, 1,
            "the table page is the only metadata read"
        );
        assert_eq!(stats.io.page_writes, 0);
        assert_eq!(stats.pressure_checkpoints, 0);
        volume.close()?;
    }
    Ok(())
}

#[test]
fn xxh3_matches_persistent_format_vectors() {
    assert_eq!(xxh3_64(b""), 0x2d06_8005_38d3_94c2);
    assert_eq!(xxh3_64(&[0; BLOCK_SIZE]), 0x93d7_6fe1_48c6_89ba);

    let mut payload_input = [0xa5; BLOCK_SIZE + 8];
    payload_input[..4].copy_from_slice(&0x0102_0304_u32.to_le_bytes());
    payload_input[4..8].copy_from_slice(&0x0506_0708_u32.to_le_bytes());
    assert_eq!(
        xxh3_64_with_seed(&payload_input, 0x1122_3344_5566_7788),
        0x4681_58e2_0c4c_72c4
    );
}

#[test]
fn format_validates_geometry_boundaries() -> io::Result<()> {
    assert!(Layout::new(0, 100, 8).is_err());
    assert!(Layout::new(1, 2 + 3 * 8, 8).is_ok());
    assert!(Layout::new(1, 2 + 3 * 8 - 1, 8).is_err());
    assert!(Layout::new(1, MAX_BACKING_BLOCKS + 1, 8).is_err());
    assert!(Layout::new(1, 2 + 3 * 4, 3).is_err());

    let missing = TemporaryBacking::new()?;
    assert_eq!(
        format(&missing.0, 0).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        format(&missing.0, BLOCK_SIZE as u64 - 1)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        format(&missing.0, (MAX_VOLUME_BLOCKS + 1) * BLOCK_SIZE as u64)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(!missing.0.exists());
    let too_small = TemporaryBacking::new()?;
    too_small.create_sized((2 + 2 * 8) * BLOCK_SIZE as u64)?;
    assert_eq!(
        format_with_options(
            &too_small.0,
            BLOCK_SIZE as u64,
            FormatOptions {
                blocks_per_segment: 8
            }
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::InvalidInput
    );
    // The default segment size needs three 8 MiB segments.
    assert_eq!(
        format(&too_small.0, BLOCK_SIZE as u64).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let misaligned = TemporaryBacking::new()?;
    misaligned.create_sized((2 + 3 * 8) * BLOCK_SIZE as u64 - 1)?;
    assert_eq!(
        format_with_options(
            &misaligned.0,
            BLOCK_SIZE as u64,
            FormatOptions {
                blocks_per_segment: 8
            }
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::InvalidInput
    );
    Ok(())
}

#[test]
fn open_reconstructs_empty_volume_and_requires_matching_roots() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 1025_u32;
    let layout = format_segments(&backing, volume_blocks, 3, 4)?;
    let backing_blocks = layout.backing_blocks;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    let volume_id = read_u64(&descriptors, 8);
    let blue_descriptor = descriptors[BLOCK_SIZE..].to_vec();

    let volume = open(&backing.0)?;
    assert_eq!(volume.log.volume_id, volume_id);
    assert_eq!(volume.volume_blocks(), volume_blocks);
    assert_eq!(volume.log.geometry.backing_blocks, backing_blocks);
    assert!(mapping_arrays(&volume)?.0.iter().all(|block| *block == 0));
    assert!(
        mapping_arrays(&volume)?
            .1
            .iter()
            .all(|checksum| *checksum == 0)
    );
    assert_eq!(last_lsn(&volume), FORMAT_LSN);
    assert_eq!(volume.log.checkpoint_lsn(), FORMAT_LSN);
    assert_eq!(user_position(&volume).boundary, 2);
    assert_eq!(volume.log.active_slot, CheckpointSlot::Green);
    assert_eq!(log_bytes_since_checkpoint(&volume), 0);
    drop(volume);

    let green = &mut descriptors[..BLOCK_SIZE];
    green[0] ^= 1;
    file.write_all_at(green, 0)?;
    let volume = open(&backing.0)?;
    assert_eq!(volume.log.active_slot, CheckpointSlot::Blue);
    assert_eq!(volume.log.root_lsn, [None, Some(FORMAT_LSN)]);
    drop(volume);

    green.copy_from_slice(&blue_descriptor);
    green[5] = CheckpointSlot::Green as u8;
    green[8] ^= 1;
    green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
    let checksum = xxh3_64(green);
    green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    file.write_all_at(green, 0)?;
    assert_eq!(
        open(&backing.0).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );

    Ok(())
}

#[test]
fn format_writes_valid_empty_checkpoint_roots() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 1025_u32;
    let segments = 3_u64;
    let blocks_per_segment = 4_u32;
    backing.create_sized((2 + segments * u64::from(blocks_per_segment)) * BLOCK_SIZE as u64)?;
    let stale = vec![0xa5; (segments as usize * blocks_per_segment as usize) * BLOCK_SIZE];
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .write_all_at(&stale, 2 * BLOCK_SIZE as u64)?;
    format_with_options(
        &backing.0,
        u64::from(volume_blocks) * BLOCK_SIZE as u64,
        FormatOptions { blocks_per_segment },
    )?;
    let backing_blocks = 2 + segments * u64::from(blocks_per_segment);

    let file = File::open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    let volume_id = read_u64(&descriptors, 8);

    for index in 0..2 {
        let descriptor = &descriptors[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE];
        assert_eq!(&descriptor[..4], CHECKPOINT_MAGIC);
        assert_eq!(descriptor[4], FORMAT_VERSION);
        assert_eq!(descriptor[5], index as u8);
        assert_eq!(&descriptor[6..8], &[0, 0]);
        assert_eq!(read_u64(descriptor, 8), volume_id);
        assert_eq!(read_u32(descriptor, 16), BLOCK_SIZE as u32);
        assert_eq!(read_u32(descriptor, 20), volume_blocks);
        assert_eq!(read_u64(descriptor, 24), backing_blocks);
        assert_eq!(read_u64(descriptor, 32), FORMAT_LSN);
        assert!(descriptor[40..48].iter().all(|byte| *byte == 0));
        assert_eq!(read_u32(descriptor, 48), blocks_per_segment);
        assert_eq!(read_u32(descriptor, 52), 2);
        assert_eq!(read_u32(descriptor, 56), 0);
        assert!(descriptor[60..348].iter().all(|byte| *byte == 0));
        assert_eq!(read_u32(descriptor, 348), 1);
        assert_eq!(read_u32(descriptor, 352), 2);
        assert!(
            descriptor[356..DESCRIPTOR_CHECKSUM_OFFSET]
                .iter()
                .all(|byte| *byte == 0)
        );

        let expected_checksum = read_u64(descriptor, DESCRIPTOR_CHECKSUM_OFFSET);
        let mut checksummed = descriptor.to_vec();
        checksummed[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        assert_eq!(xxh3_64(&checksummed), expected_checksum);
    }

    // Format writes only the table page; stale bytes stay until open sanitizes both heads.
    let mut table_page = [0; BLOCK_SIZE];
    file.read_exact_at(&mut table_page, 2 * BLOCK_SIZE as u64)?;
    assert_eq!(&table_page[..4], FOOTER_MAGIC);
    assert_eq!(table_page[5], 3);
    let mut untouched = [0; BLOCK_SIZE];
    file.read_exact_at(&mut untouched, 3 * BLOCK_SIZE as u64)?;
    assert_eq!(untouched, [0xa5; BLOCK_SIZE]);
    let volume = open(&backing.0)?;
    assert_eq!(volume.log.active_slot, CheckpointSlot::Green);
    drop(volume);
    let mut sanitized = vec![1; 2 * blocks_per_segment as usize * BLOCK_SIZE - BLOCK_SIZE];
    file.read_exact_at(&mut sanitized, 3 * BLOCK_SIZE as u64)?;
    assert!(sanitized.iter().all(|byte| *byte == 0));
    let mut free_segment = [0; BLOCK_SIZE];
    file.read_exact_at(
        &mut free_segment,
        (2 + 2 * u64::from(blocks_per_segment)) * BLOCK_SIZE as u64,
    )?;
    assert_eq!(free_segment, [0xa5; BLOCK_SIZE]);

    for descriptor in descriptors.chunks_exact_mut(BLOCK_SIZE) {
        descriptor[6] = 1;
        descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        let checksum = xxh3_64(descriptor);
        descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    }
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .write_all_at(&descriptors, 0)?;
    assert_eq!(
        open(&backing.0).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[test]
fn format_three_images_are_rejected_without_modification() -> io::Result<()> {
    // A captured format-3 descriptor pair from v1.2.3 (green slot, then blue), sealed with the
    // inherited full-block checksum. Only the version byte and layout differ from format 4.
    let backing = TemporaryBacking::new()?;
    backing.create_sized(64 * BLOCK_SIZE as u64)?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    for slot in 0..2_u8 {
        let mut descriptor = [0; BLOCK_SIZE];
        descriptor[..4].copy_from_slice(b"VBLC");
        descriptor[4] = 3;
        descriptor[5] = slot;
        descriptor[8..16].copy_from_slice(&19_u64.to_le_bytes());
        descriptor[16..20].copy_from_slice(&4096_u32.to_le_bytes());
        descriptor[20..24].copy_from_slice(&1_u32.to_le_bytes());
        descriptor[24..32].copy_from_slice(&64_u64.to_le_bytes());
        descriptor[40..44].copy_from_slice(&3_u32.to_le_bytes());
        descriptor[44..52].copy_from_slice(&xxh3_64(&[0; BLOCK_SIZE]).to_le_bytes());
        descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        let checksum = xxh3_64(&descriptor);
        descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        file.write_all_at(&descriptor, u64::from(slot) * BLOCK_SIZE as u64)?;
    }
    file.sync_all()?;
    let before = std::fs::read(&backing.0)?;
    for _ in 0..2 {
        assert_eq!(
            open(&backing.0).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&backing.0)?, before);
    }
    Ok(())
}
