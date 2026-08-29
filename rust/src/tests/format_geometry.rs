use super::*;

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
    assert!(layout_for(0).is_err());
    assert!(layout_for(BLOCK_SIZE as u64 - 1).is_err());
    assert!(layout_for((MAX_VOLUME_BLOCKS + 1) * BLOCK_SIZE as u64).is_err());

    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    assert_eq!(
        backing_blocks_for(&layout, MAX_BACKING_BLOCKS * BLOCK_SIZE as u64)?,
        MAX_BACKING_BLOCKS
    );
    assert!(backing_blocks_for(&layout, (MAX_BACKING_BLOCKS + 1) * BLOCK_SIZE as u64).is_err());
    assert!(backing_blocks_for(&layout, u64::from(layout.log_start + 2) * 4096 - 1).is_err());
    assert!(backing_blocks_for(&layout, u64::from(layout.log_start + 1) * 4096).is_err());

    let missing = TemporaryBacking::new()?;
    assert_eq!(
        format(&missing.0, 0).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let too_small = TemporaryBacking::new()?;
    too_small.create_sized(u64::from(layout.log_start + 1) * BLOCK_SIZE as u64)?;
    assert_eq!(
        format(&too_small.0, BLOCK_SIZE as u64).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let misaligned = TemporaryBacking::new()?;
    misaligned.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64 - 1)?;
    assert_eq!(
        format(&misaligned.0, BLOCK_SIZE as u64).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    Ok(())
}

#[test]
fn open_reconstructs_empty_volume_and_requires_matching_roots() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let volume_blocks = 1025_u32;
    let backing_blocks = 14_u64;
    backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut descriptors = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut descriptors, 0)?;
    let volume_id = read_u64(&descriptors, 8);
    let blue_descriptor = descriptors[BLOCK_SIZE..].to_vec();

    let volume = open(&backing.0)?;
    assert_eq!(volume.volume_id, volume_id);
    assert_eq!(volume.volume_blocks, volume_blocks);
    assert_eq!(volume.backing_blocks, backing_blocks);
    assert!(volume.physical_blocks.iter().all(|block| *block == 0));
    assert!(volume.checksums.iter().all(|checksum| *checksum == 0));
    assert_eq!(volume.last_lsn, 0);
    assert_eq!(volume.durable_lsn, 0);
    assert_eq!(volume.checkpoint_lsn, 0);
    assert_eq!(volume.last_footer_block, 11);
    assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);
    assert_eq!(volume.log_bytes_since_checkpoint, 0);
    drop(volume);

    let green = &mut descriptors[..BLOCK_SIZE];
    green[0] ^= 1;
    file.write_all_at(green, 0)?;
    let volume = open(&backing.0)?;
    assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Green);
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
    let backing_blocks = 14_u64;
    backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let checkpoint_bodies = vec![0xa5; (layout.log_start as usize - 2) * BLOCK_SIZE];
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .write_all_at(&checkpoint_bodies, 2 * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

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
        assert_eq!(read_u64(descriptor, 32), 0);
        assert_eq!(read_u32(descriptor, 40), 11);
        assert_eq!(read_u64(descriptor, CHECKPOINT_BODY_CHECKSUM_OFFSET), 0);
        assert!(
            descriptor[CHECKPOINT_BODY_CHECKSUM_OFFSET + 8..DESCRIPTOR_CHECKSUM_OFFSET]
                .iter()
                .all(|byte| *byte == 0)
        );

        let expected_checksum = read_u64(descriptor, DESCRIPTOR_CHECKSUM_OFFSET);
        let mut checksummed = descriptor.to_vec();
        checksummed[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        assert_eq!(xxh3_64(&checksummed), expected_checksum);
    }

    let mut preserved_bodies = vec![0; checkpoint_bodies.len()];
    file.read_exact_at(&mut preserved_bodies, 2 * BLOCK_SIZE as u64)?;
    assert_eq!(preserved_bodies, checkpoint_bodies);
    let volume = open(&backing.0)?;
    assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);
    drop(volume);

    let mut empty_log = [1; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut empty_log, 12 * BLOCK_SIZE as u64)?;
    assert!(empty_log.iter().all(|byte| *byte == 0));

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
