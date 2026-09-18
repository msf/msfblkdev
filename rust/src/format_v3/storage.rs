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
            write_block(
                &mut backing_io,
                &backing,
                slot.body_start(geometry) + block,
                &bytes,
            )?;
        }
    }
    sync(&mut backing_io, &backing)?;
    for slot in [CheckpointSlot::Green, CheckpointSlot::Blue] {
        checkpoint.descriptor.slot = slot;
        checkpoint.descriptor.encode(&mut bytes.0)?;
        write_block(&mut backing_io, &backing, slot as u32, &bytes)?;
    }
    sync(&mut backing_io, &backing)
}

pub(crate) fn write_block(
    backing_io: &mut BackingIo,
    backing: &File,
    physical_block: u32,
    block: &AlignedBlock,
) -> io::Result<()> {
    let write = opcode::Write::new(
        types::Fd(backing.as_raw_fd()),
        block.0.as_ptr(),
        BLOCK_SIZE as u32,
    )
    .offset(u64::from(physical_block) * BLOCK_SIZE as u64)
    .build();
    submit_exact(
        backing_io,
        write,
        u64::from(physical_block),
        BLOCK_SIZE as i32,
    )
}

pub(crate) fn sync(backing_io: &mut BackingIo, backing: &File) -> io::Result<()> {
    let sync = opcode::Fsync::new(types::Fd(backing.as_raw_fd())).build();
    submit_exact(backing_io, sync, 0, 0)
}
