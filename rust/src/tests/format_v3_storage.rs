use super::*;
use crate::format_v3::{
    checkpoint::{CheckpointDescriptor, CheckpointSlot},
    geometry::Geometry,
    index::Checkpoint,
    storage,
};

#[test]
fn format_v3_persists_both_full_index_bodies_and_roots() -> io::Result<()> {
    let geometry = Geometry::new(320 * 1024 + 1, 32)?;
    let backing = TemporaryBacking::new()?;
    backing.create_sized(32 * BLOCK_SIZE as u64)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    file.write_all_at(&[0xa5; 7 * BLOCK_SIZE], 0)?;
    storage::format(
        &backing.0,
        u64::from(geometry.volume_blocks) * BLOCK_SIZE as u64,
        None,
    )?;
    let mut bytes = [0; BLOCK_SIZE];
    let mut descriptors = Vec::new();
    for slot in [CheckpointSlot::Green, CheckpointSlot::Blue] {
        file.read_exact_at(&mut bytes, slot as u64 * BLOCK_SIZE as u64)?;
        let descriptor = CheckpointDescriptor::decode(&bytes, slot, 32)?;
        assert_eq!(descriptor.geometry()?, geometry);
        assert_eq!(descriptor.generation, 0);
        assert_eq!(descriptor.checkpoint_lsn, 0);
        assert_eq!(descriptor.last_record_boundary, geometry.log_start - 1);
        let mut checkpoint = Checkpoint::empty(geometry, descriptor.volume_id)?;
        checkpoint.descriptor = descriptor;
        for block in 0..geometry.index_blocks {
            file.read_exact_at(
                &mut bytes,
                u64::from(slot.body_start(geometry) + block) * BLOCK_SIZE as u64,
            )?;
            assert_eq!(bytes, [0; BLOCK_SIZE]);
            checkpoint.decode_block(block, &bytes)?;
        }
        assert_eq!(checkpoint.checksum()?, descriptor.index_checksum);
        assert_ne!(descriptor.index_checksum, 0);
        descriptors.push(descriptor);
    }
    descriptors[0].validate_pair(descriptors[1])?;
    file.read_exact_at(
        &mut bytes,
        u64::from(geometry.log_start) * BLOCK_SIZE as u64,
    )?;
    assert_eq!(
        bytes, [0xa5; BLOCK_SIZE],
        "formatter must not write the log"
    );
    assert_eq!(file.metadata()?.len(), 32 * BLOCK_SIZE as u64);
    Ok(())
}

#[test]
fn format_v3_rejects_invalid_geometry_without_writing() -> io::Result<()> {
    for (volume_bytes, backing_bytes) in [
        (0, 32768),
        (4097, 32768),
        ((MAX_VOLUME_BLOCKS + 1) * 4096, 32768),
        (4096, 6 * 4096),
        (4096, 32767),
    ] {
        let backing = TemporaryBacking::new()?;
        backing.create_sized(backing_bytes)?;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let original = vec![0xa5; backing_bytes as usize];
        file.write_all_at(&original, 0)?;
        assert_eq!(
            storage::format(&backing.0, volume_bytes, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let mut after = vec![0; original.len()];
        file.read_exact_at(&mut after, 0)?;
        assert_eq!(original, after);
    }
    Ok(())
}
