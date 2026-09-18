use super::{
    checkpoint::{CheckpointDescriptor, CheckpointSlot},
    index::Checkpoint,
    mapping_page::{MappingPageRecord, PageExpectation},
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAX_BACKING_BLOCKS, accounted_io::BackingIo, invalid_data,
    read_block_exact,
};
use std::{fs::File, io};
use xxhash_rust::xxh3::Xxh3;

pub(crate) fn load(
    backing_io: &mut BackingIo,
    backing: &File,
    backing_blocks: u64,
) -> io::Result<(Checkpoint, u64)> {
    if !(2..=MAX_BACKING_BLOCKS).contains(&backing_blocks) {
        return Err(invalid_data("invalid checkpoint backing size"));
    }
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    let mut candidates = [None, None];
    for (candidate, slot) in candidates
        .iter_mut()
        .zip([CheckpointSlot::Green, CheckpointSlot::Blue])
    {
        read_block_exact(backing_io, backing, slot as u32, &mut bytes)?;
        match CheckpointDescriptor::decode(&bytes.0, slot, backing_blocks) {
            Ok(descriptor) => *candidate = Some(descriptor),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            Err(error) => return Err(error),
        }
    }
    if let [Some(green), Some(blue)] = candidates {
        green.validate_pair(blue)?;
        if blue.generation > green.generation {
            candidates.swap(0, 1);
        }
    }
    for descriptor in candidates.into_iter().flatten() {
        match load_candidate(backing_io, backing, descriptor, &mut bytes) {
            Ok(checkpoint) => return Ok(checkpoint),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            Err(error) => return Err(error),
        }
    }
    Err(invalid_data("no valid version-3 checkpoint"))
}

fn load_candidate(
    backing_io: &mut BackingIo,
    backing: &File,
    descriptor: CheckpointDescriptor,
    bytes: &mut AlignedBlock,
) -> io::Result<(Checkpoint, u64)> {
    let geometry = descriptor.geometry()?;
    let mut checkpoint = Checkpoint::empty(geometry, descriptor.volume_id)?;
    checkpoint.descriptor = descriptor;
    let mut hasher = Xxh3::new();
    for block in 0..geometry.index_blocks {
        read_block_exact(
            backing_io,
            backing,
            descriptor.slot.body_start(geometry) + block,
            bytes,
        )?;
        checkpoint.decode_block(block, &bytes.0)?;
        hasher.update(&bytes.0);
    }
    if hasher.digest() != descriptor.index_checksum {
        return Err(invalid_data("checkpoint index checksum mismatch"));
    }
    let mut live_blocks = 0;
    for (page_id, &boundary) in checkpoint.mapping_page_block.iter().enumerate() {
        if boundary == 0 {
            continue;
        }
        read_block_exact(backing_io, backing, boundary, bytes)?;
        let page = MappingPageRecord::decode(
            &bytes.0,
            &PageExpectation {
                geometry,
                volume_id: descriptor.volume_id,
                page_id: page_id as u32,
                boundary,
                maximum_lsn: descriptor.checkpoint_lsn,
            },
        )?;
        live_blocks += page
            .entries
            .physical_addr
            .iter()
            .filter(|&&address| address != 0)
            .count() as u64;
    }
    Ok((checkpoint, live_blocks))
}
