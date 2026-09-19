use super::{checkpoint::CheckpointSlot, geometry::Geometry, index::Checkpoint};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAX_VOLUME_BLOCKS,
    accounted_io::{BackingIo, submit_exact},
    invalid_input, io_accounting, random_volume_id,
};
use io_uring::{opcode, types};
use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
};

// Match v1.1's bounded 128 KiB checkpoint submissions.
pub(crate) const CHECKPOINT_BATCH_BLOCKS: usize = 32;

pub(crate) fn format(
    path: &Path,
    volume_bytes: u64,
    accounting: Option<io_accounting::Writer>,
) -> io::Result<()> {
    if volume_bytes == 0
        || !volume_bytes.is_multiple_of(BLOCK_SIZE as u64)
        || volume_bytes / BLOCK_SIZE as u64 > MAX_VOLUME_BLOCKS
    {
        return Err(invalid_input("invalid volume size"));
    }
    let mut backing = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(path)?;
    let backing_bytes = backing.seek(SeekFrom::End(0))?;
    if !backing_bytes.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(invalid_input("backing size is not block aligned"));
    }
    let geometry = Geometry::new(
        (volume_bytes / BLOCK_SIZE as u64) as u32,
        backing_bytes / BLOCK_SIZE as u64,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut checkpoint = Checkpoint::empty(geometry, random_volume_id()?)?;
    let mut backing_io = BackingIo::new(accounting)?;
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    for slot in [CheckpointSlot::Green, CheckpointSlot::Blue] {
        for block in 0..geometry.index_blocks {
            checkpoint.encode_block(block, &mut bytes.0)?;
            write_blocks(
                &mut backing_io,
                &backing,
                slot.body_start(geometry) + block,
                std::slice::from_ref(&bytes),
            )?;
        }
    }
    sync(&mut backing_io, &backing)?;
    for slot in [CheckpointSlot::Green, CheckpointSlot::Blue] {
        checkpoint.descriptor.slot = slot;
        checkpoint.descriptor.encode(&mut bytes.0)?;
        write_blocks(
            &mut backing_io,
            &backing,
            slot as u32,
            std::slice::from_ref(&bytes),
        )?;
    }
    sync(&mut backing_io, &backing)
}

pub(crate) fn write_blocks(
    backing_io: &mut BackingIo,
    backing: &File,
    physical_block: u32,
    blocks: &[AlignedBlock],
) -> io::Result<()> {
    assert!(!blocks.is_empty() && blocks.len() <= CHECKPOINT_BATCH_BLOCKS);
    assert_eq!(
        size_of::<AlignedBlock>(),
        BLOCK_SIZE,
        "contiguous aligned blocks"
    );
    assert!(u64::from(physical_block) + blocks.len() as u64 <= crate::MAX_BACKING_BLOCKS);
    let expected = blocks.len() * BLOCK_SIZE;
    let submitted = expected;
    #[cfg(test)]
    let submitted = match backing_io.metadata_short_write {
        Some((at, limit)) if at == physical_block => limit.min(expected),
        _ => submitted,
    };
    let write = opcode::Write::new(
        types::Fd(backing.as_raw_fd()),
        blocks.as_ptr().cast::<u8>(),
        submitted as u32,
    )
    .offset(u64::from(physical_block) * BLOCK_SIZE as u64)
    .build();
    submit_exact(
        backing_io,
        write,
        u64::from(physical_block),
        expected as i32,
    )
}

pub(crate) fn sync(backing_io: &mut BackingIo, backing: &File) -> io::Result<()> {
    let sync = opcode::Fsync::new(types::Fd(backing.as_raw_fd())).build();
    submit_exact(backing_io, sync, 0, 0)
}
