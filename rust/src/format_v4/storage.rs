//! Backing I/O helpers and the format-4 formatter.
use super::{
    Geometry, HeadId, RecordKind,
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    geometry::{DIRECT_POINTERS, INDIRECT_POINTERS, TABLE_POINTERS},
    heads::Head,
    segments::Segments,
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAX_VOLUME_BLOCKS,
    accounted_io::{BackingIo, submit_exact},
    backing::{self, Claim, OpenMode, Opened},
    invalid_input, io_accounting, random_volume_id,
};
use io_uring::{opcode, types};
use std::{
    fs::File,
    io::{self, Seek, SeekFrom},
    os::fd::AsRawFd,
    path::Path,
};

/// Consecutive one-block records per positional write (128 KiB), as in format 3.
pub(crate) const BATCH_BLOCKS: usize = 32;

/// Format-time choices. `blocks_per_segment` of zero selects the largest eligible size for 100% logical fill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatOptions {
    pub blocks_per_segment: u32,
}

/// Opens and claims the backing for writing; see [`crate::open_live`] for the claim.
/// Returns the backing, its whole blocks and the claim, which must outlive every use of it.
pub(crate) fn open_backing(path: &Path) -> io::Result<(File, u64, Claim)> {
    let Opened {
        file: mut backing,
        claim,
        ..
    } = backing::open(path, OpenMode::Live)?;
    let bytes = backing.seek(SeekFrom::End(0))?;
    if !bytes.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(invalid_input("backing size is not block aligned"));
    }
    Ok((backing, bytes / BLOCK_SIZE as u64, claim))
}

/// Writes the initial segment table pages into segment 0 and both descriptors.
pub(crate) fn format(
    path: &Path,
    volume_bytes: u64,
    options: FormatOptions,
    accounting: Option<io_accounting::Writer>,
) -> io::Result<()> {
    format_with_io(path, volume_bytes, options, random_volume_id, || {
        BackingIo::new(accounting)
    })
}

pub(crate) fn format_with_io(
    path: &Path,
    volume_bytes: u64,
    options: FormatOptions,
    volume_id: impl FnOnce() -> io::Result<u64>,
    make_io: impl FnOnce() -> io::Result<BackingIo>,
) -> io::Result<()> {
    if volume_bytes == 0
        || !volume_bytes.is_multiple_of(BLOCK_SIZE as u64)
        || volume_bytes / BLOCK_SIZE as u64 > MAX_VOLUME_BLOCKS
    {
        return Err(invalid_input("invalid volume size"));
    }
    let (backing, backing_blocks, _claim) = open_backing(path)?;
    let blocks_per_segment = if options.blocks_per_segment == 0 {
        (crate::plan_format(volume_bytes, backing_blocks * BLOCK_SIZE as u64, 100)?.segment_bytes
            / BLOCK_SIZE as u64) as u32
    } else {
        options.blocks_per_segment
    };
    let geometry = Geometry::new(
        (volume_bytes / BLOCK_SIZE as u64) as u32,
        backing_blocks,
        blocks_per_segment,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if u64::from(geometry.table_pages) + 1 > u64::from(blocks_per_segment) {
        return Err(invalid_input(
            "a segment must hold every table page plus its link",
        ));
    }
    let volume_id = volume_id()?;
    let mut segments = Segments::new(geometry, geometry.segment_count)?;
    let user = segments.allocate(HeadId::User)?;
    let relocation = segments.allocate(HeadId::Relocation)?;
    assert_eq!((user, relocation), (0, 1), "format opens segments 0 and 1");
    let mut head = Head::opened(HeadId::User, user, geometry)?;
    let mut backing_io = make_io()?;
    let mut block = AlignedBlock([0; BLOCK_SIZE]);
    let mut descriptor = CheckpointDescriptor {
        slot: CheckpointSlot::Green,
        volume_id,
        volume_blocks: geometry.volume_blocks,
        backing_blocks,
        checkpoint_lsn: 0,
        blocks_per_segment,
        head_boundary: [0, 0],
        direct_pages: [0; DIRECT_POINTERS],
        indirect_pages: [0; INDIRECT_POINTERS],
        table_pages: [0; TABLE_POINTERS],
    };
    let mut lsn = 1;
    for table in 0..geometry.table_pages {
        let header = head.header(RecordKind::SegmentTablePage, volume_id, lsn, 0);
        segments
            .encode_table_page(header, table)?
            .encode(geometry, &mut block.0)?;
        write_blocks(
            &mut backing_io,
            &backing,
            header.boundary,
            std::slice::from_ref(&block),
        )?;
        descriptor.table_pages[table as usize] = header.boundary;
        head.advance(header);
        lsn += 1;
    }
    sync(&mut backing_io, &backing)?;
    descriptor.checkpoint_lsn = lsn - 1;
    descriptor.head_boundary = [head.last_boundary, 0];
    for slot in CheckpointSlot::BOTH {
        descriptor.slot = slot;
        descriptor.encode(&mut block.0)?;
        write_blocks(
            &mut backing_io,
            &backing,
            slot.block(),
            std::slice::from_ref(&block),
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
    assert!(!blocks.is_empty() && blocks.len() <= BATCH_BLOCKS);
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

/// Zeroes `count` blocks from `start` in batches of `BATCH_BLOCKS`. The caller persists them.
pub(crate) fn zero_blocks(
    backing_io: &mut BackingIo,
    backing: &File,
    start: u64,
    count: u64,
) -> io::Result<()> {
    let zero = [const { AlignedBlock([0; BLOCK_SIZE]) }; BATCH_BLOCKS];
    let mut offset = 0;
    while offset < count {
        let batch = (count - offset).min(BATCH_BLOCKS as u64);
        let block = start + offset;
        let write = opcode::Write::new(
            types::Fd(backing.as_raw_fd()),
            zero.as_ptr().cast::<u8>(),
            (batch as usize * BLOCK_SIZE) as u32,
        )
        .offset(block * BLOCK_SIZE as u64)
        .build();
        submit_exact(
            backing_io,
            write,
            block,
            (batch as usize * BLOCK_SIZE) as i32,
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_indexed_failpoint(
            crate::STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT,
            offset / BATCH_BLOCKS as u64,
        )?;
        offset += batch;
    }
    Ok(())
}
