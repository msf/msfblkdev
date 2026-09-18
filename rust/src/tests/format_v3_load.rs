use super::*;
use crate::format_v3::{
    RecordHeader, RecordKind,
    checkpoint::{CheckpointDescriptor, CheckpointSlot},
    checkpoint_load,
    data_record::DataRecord,
    geometry::Geometry,
    index::Checkpoint,
    mapping_page::MappingPageRecord,
    seal_metadata, storage,
};
use crate::mapping_cache::MappingEntries;

fn write_checkpoint(file: &File, checkpoint: &Checkpoint) -> io::Result<()> {
    let geometry = checkpoint.descriptor.geometry()?;
    let mut bytes = [0; BLOCK_SIZE];
    for block in 0..geometry.index_blocks {
        checkpoint.encode_block(block, &mut bytes)?;
        file.write_all_at(
            &bytes,
            u64::from(checkpoint.descriptor.slot.body_start(geometry) + block) * BLOCK_SIZE as u64,
        )?;
    }
    checkpoint.descriptor.encode(&mut bytes)?;
    file.write_all_at(
        &bytes,
        checkpoint.descriptor.slot as u64 * BLOCK_SIZE as u64,
    )
}

pub(super) fn populated_fixture(volume_blocks: u32) -> io::Result<(TemporaryBacking, Checkpoint)> {
    let geometry = Geometry::new(volume_blocks, 32)?;
    let backing = TemporaryBacking::new()?;
    backing.create_sized(32 * BLOCK_SIZE as u64)?;
    storage::format(
        &backing.0,
        u64::from(volume_blocks) * BLOCK_SIZE as u64,
        None,
    )?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut bytes = [0; BLOCK_SIZE];
    file.read_exact_at(&mut bytes, BLOCK_SIZE as u64)?;
    let descriptor = CheckpointDescriptor::decode(&bytes, CheckpointSlot::Blue, 32)?;
    let mut checkpoint = Checkpoint::empty(geometry, descriptor.volume_id)?;
    checkpoint.descriptor = descriptor;
    let lba = volume_blocks - 1;
    let payload = [0x5a; BLOCK_SIZE];
    let checksum = payload_checksum(descriptor.volume_id, lba, geometry.log_start, &payload);
    file.write_all_at(&payload, u64::from(geometry.log_start) * BLOCK_SIZE as u64)?;
    let mut record = DataRecord {
        header: RecordHeader {
            kind: RecordKind::Data,
            volume_id: descriptor.volume_id,
            lsn: 1,
            previous_boundary: geometry.log_start - 1,
            boundary: geometry.log_start + 1,
        },
        lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
        checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
    };
    record.lba_ids[0] = lba;
    record.checksums[0] = checksum;
    record.encode(geometry, &mut bytes)?;
    file.write_all_at(
        &bytes,
        u64::from(geometry.log_start + 1) * BLOCK_SIZE as u64,
    )?;
    let mut mapping = MappingPageRecord {
        header: RecordHeader {
            kind: RecordKind::MappingPage,
            lsn: 2,
            previous_boundary: geometry.log_start + 1,
            boundary: geometry.log_start + 2,
            ..record.header
        },
        page_id: lba / 320,
        entries: MappingEntries::zeroed(),
    };
    mapping.entries.physical_addr[(lba % 320) as usize] = geometry.log_start;
    mapping.entries.payload_checksum[(lba % 320) as usize] = checksum;
    mapping.encode(geometry, &mut bytes)?;
    file.write_all_at(
        &bytes,
        u64::from(geometry.log_start + 2) * BLOCK_SIZE as u64,
    )?;
    checkpoint.descriptor.generation = 1;
    checkpoint.descriptor.checkpoint_lsn = 2;
    checkpoint.descriptor.last_record_boundary = geometry.log_start + 2;
    checkpoint.mapping_page_block[mapping.page_id as usize] = mapping.header.boundary;
    checkpoint.descriptor.index_checksum = checkpoint.checksum()?;
    write_checkpoint(&file, &checkpoint)?;
    Ok((backing, checkpoint))
}

fn load(backing: &TemporaryBacking) -> io::Result<Checkpoint> {
    let file = backing.reopen()?;
    checkpoint_load::load(
        &mut BackingIo::new(None)?,
        &file,
        file.metadata()?.len() / BLOCK_SIZE as u64,
    )
    .map(|(checkpoint, _)| checkpoint)
}

#[test]
fn checkpoint_load_validates_multiblock_index_and_last_partial_page() -> io::Result<()> {
    let (backing, expected) = populated_fixture(320 * 1024 + 1)?;
    let selected = load(&backing)?;
    assert_eq!(selected.descriptor, expected.descriptor);
    assert_eq!(selected.mapping_page_block, expected.mapping_page_block);
    assert_eq!(selected.allocated_bytes(), 1025 * 4);
    Ok(())
}

#[test]
fn checkpoint_load_falls_back_after_referenced_page_corruption() -> io::Result<()> {
    let (backing, checkpoint) = populated_fixture(321)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let offset = u64::from(checkpoint.descriptor.last_record_boundary) * BLOCK_SIZE as u64;
    let mut original = [0; BLOCK_SIZE];
    file.read_exact_at(&mut original, offset)?;
    for (offset_in_page, value, reseal) in
        [(40, 1, false), (32, 0, true), (16, 3, true), (5, 99, true)]
    {
        let mut corrupt = original;
        corrupt[offset_in_page] = value;
        if reseal {
            seal_metadata(&mut corrupt);
        }
        file.write_all_at(&corrupt, offset)?;
        let selected = load(&backing)?;
        assert_eq!(selected.descriptor.slot, CheckpointSlot::Green);
        assert_eq!(selected.descriptor.generation, 0);
        assert!(
            selected
                .mapping_page_block
                .iter()
                .all(|pointer| *pointer == 0)
        );
    }
    file.write_all_at(&original, offset)?;
    assert_eq!(load(&backing)?.descriptor.generation, 1);
    Ok(())
}

#[test]
fn checkpoint_load_rejects_invalid_bodies_and_conflicting_roots() -> io::Result<()> {
    let (backing, mut checkpoint) = populated_fixture(321)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let geometry = checkpoint.descriptor.geometry()?;
    let body = u64::from(CheckpointSlot::Blue.body_start(geometry)) * BLOCK_SIZE as u64;
    for (offset, value) in [(0, 1), (4, 5), (8, 1), (BLOCK_SIZE as u64 - 1, 1)] {
        file.write_all_at(&[value], body + offset)?;
        assert_eq!(load(&backing)?.descriptor.generation, 0);
        write_checkpoint(&file, &checkpoint)?;
    }
    let mut green = [0; BLOCK_SIZE];
    file.read_exact_at(&mut green, 0)?;
    green[0] = 0;
    file.write_all_at(&green, 0)?;
    assert_eq!(load(&backing)?.descriptor.generation, 1);
    file.write_all_at(&[1], body)?;
    assert_eq!(
        load(&backing).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    write_checkpoint(&file, &checkpoint)?;
    checkpoint.descriptor.slot = CheckpointSlot::Green;
    checkpoint.descriptor.volume_id ^= 1;
    write_checkpoint(&file, &checkpoint)?;
    assert_eq!(
        load(&backing).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    checkpoint.descriptor.volume_id ^= 1;
    checkpoint.descriptor.checkpoint_lsn = 1;
    write_checkpoint(&file, &checkpoint)?;
    assert_eq!(
        load(&backing).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[test]
fn checkpoint_load_propagates_failed_reads_without_selecting_an_older_root() -> io::Result<()> {
    let (backing, checkpoint) = populated_fixture(321)?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    // Simulate loss of the snapshot after the caller has obtained backing geometry.
    file.set_len(u64::from(checkpoint.descriptor.last_record_boundary) * BLOCK_SIZE as u64)?;
    let error = checkpoint_load::load(&mut BackingIo::new(None)?, &backing.reopen()?, 32)
        .err()
        .unwrap();
    assert_ne!(error.kind(), io::ErrorKind::InvalidData);
    Ok(())
}

#[test]
fn checkpoint_load_rejects_old_and_unknown_formats_without_writing() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(32 * BLOCK_SIZE as u64)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut original = vec![0; 32 * BLOCK_SIZE];
    original[..2 * BLOCK_SIZE].copy_from_slice(include_bytes!("fixtures/v1-empty-roots.bin"));
    assert_eq!(original[4], 1);
    for root in original[..2 * BLOCK_SIZE].chunks_exact(BLOCK_SIZE) {
        assert_eq!(&root[..4], b"VBLC");
        let checksum = read_u64(root, BLOCK_SIZE - 8);
        let mut checked: [u8; BLOCK_SIZE] = root.try_into().unwrap();
        checked[BLOCK_SIZE - 8..].fill(0);
        assert_eq!(xxhash_rust::xxh3::xxh3_64(&checked), checksum);
    }
    for version in [1, 2, 255] {
        let mut image = original.clone();
        for slot in 0..2 {
            let block: &mut [u8; BLOCK_SIZE] = (&mut image
                [slot * BLOCK_SIZE..(slot + 1) * BLOCK_SIZE])
                .try_into()
                .unwrap();
            block[4] = version;
            seal_metadata(block);
        }
        file.write_all_at(&image, 0)?;
        assert_eq!(
            load(&backing).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        let mut after = vec![0; image.len()];
        file.read_exact_at(&mut after, 0)?;
        assert_eq!(after, image);
    }
    Ok(())
}
