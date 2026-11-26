//! Root selection and complete candidate validation before any format-4 write.
use super::{
    Geometry, HeadId,
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    geometry::TABLE_ENTRIES,
    heads::Head,
    index_page::{IndexPageExpectation, MappingIndexLevel, MappingIndexPage},
    log::{Log, LogWrites},
    mapping_index::MappingIndex,
    mapping_page::{MappingPageRecord, PageExpectation},
    segment_table::{SegmentState, SegmentTablePage, TablePageExpectation},
    segments::Segments,
    storage::BATCH_BLOCKS,
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAX_BACKING_BLOCKS,
    accounted_io::{BackingIo, READ_BATCH_BLOCKS},
    invalid_data, read_block_exact,
};
use std::{fs::File, io};

pub(crate) struct Loaded {
    pub(crate) log: Log,
    /// Mapped logical blocks counted while validating every referenced snapshot.
    pub(crate) live_blocks: u64,
    /// The root that was selected.
    #[cfg(test)]
    pub(crate) descriptor: CheckpointDescriptor,
}

/// Reads both descriptors and validates candidates newest first. Never writes.
pub(crate) fn load(
    io: &mut BackingIo,
    backing: &File,
    actual_backing_blocks: u64,
    segment_capacity: impl Fn(Geometry) -> u32,
) -> io::Result<Loaded> {
    if !(2..=MAX_BACKING_BLOCKS).contains(&actual_backing_blocks) {
        return Err(invalid_data("invalid checkpoint backing size"));
    }
    let mut buffers = read_buffers()?;
    let mut candidates: [Option<CheckpointDescriptor>; 2] = [None, None];
    for (candidate, slot) in candidates.iter_mut().zip(CheckpointSlot::BOTH) {
        let block = &mut buffers[0];
        read_block_exact(io, backing, slot.block(), block)?;
        match CheckpointDescriptor::decode(&block.0, slot) {
            Ok(descriptor) => *candidate = Some(descriptor),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            Err(error) => return Err(error),
        }
    }
    if let [Some(green), Some(blue)] = &candidates {
        green.validate_pair(blue)?;
        if blue.checkpoint_lsn > green.checkpoint_lsn {
            candidates.swap(0, 1);
        }
    }
    let root_lsn = |slot: CheckpointSlot| {
        candidates
            .iter()
            .flatten()
            .find(|descriptor| descriptor.slot == slot)
            .map(|descriptor| descriptor.checkpoint_lsn)
    };
    let root_lsns = [
        root_lsn(CheckpointSlot::Green),
        root_lsn(CheckpointSlot::Blue),
    ];
    for descriptor in candidates.iter().flatten() {
        let capacity = segment_capacity(descriptor.geometry()?);
        match load_candidate(
            io,
            backing,
            descriptor,
            actual_backing_blocks,
            capacity,
            &mut buffers,
        ) {
            Ok(mut loaded) => {
                // Only the roots that decoded remain candidates for the protected LSN.
                loaded.log.root_lsn = root_lsns;
                return Ok(loaded);
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            Err(error) => return Err(error),
        }
    }
    Err(invalid_data("no valid format-4 checkpoint"))
}

/// Aligned buffers for one batch of checkpoint page reads.
pub(crate) fn read_buffers() -> io::Result<Box<[AlignedBlock]>> {
    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(READ_BATCH_BLOCKS)
        .map_err(|_| io::Error::other("read buffer allocation failed"))?;
    buffers.resize_with(READ_BATCH_BLOCKS, || AlignedBlock([0; BLOCK_SIZE]));
    Ok(buffers.into_boxed_slice())
}

/// Reads the pages that `pages` names as `(page ID, block)`, skipping zero blocks, with up to
/// `READ_BATCH_BLOCKS` reads per submission, and calls `visit` on each page's bytes. A batch is
/// read in block order, so `visit` doesn't see the pages in the order `pages` yields them.
fn read_pages(
    io: &mut BackingIo,
    backing: &File,
    buffers: &mut [AlignedBlock],
    pages: impl IntoIterator<Item = (u32, u32)>,
    mut visit: impl FnMut(u32, u32, &[u8; BLOCK_SIZE]) -> io::Result<()>,
) -> io::Result<()> {
    let mut pages = pages.into_iter().filter(|&(_, block)| block != 0);
    let mut batch = [(0_u32, 0_u32); READ_BATCH_BLOCKS];
    let mut blocks = [0_u32; READ_BATCH_BLOCKS];
    // Bounded by `pages`: every pass consumes up to one batch of them.
    loop {
        let mut count = 0;
        for (page, block) in pages.by_ref().take(READ_BATCH_BLOCKS) {
            batch[count] = (block, page);
            count += 1;
        }
        if count == 0 {
            return Ok(());
        }
        batch[..count].sort_unstable();
        for (slot, &(block, _)) in blocks.iter_mut().zip(&batch[..count]) {
            *slot = block;
        }
        io.read_blocks(backing, &blocks[..count], buffers)?;
        io.mapping_io.page_reads += count as u64;
        for (&(block, page), buffer) in batch[..count].iter().zip(buffers.iter()) {
            visit(page, block, &buffer.0)?;
        }
    }
}

pub(crate) fn load_candidate(
    io: &mut BackingIo,
    backing: &File,
    descriptor: &CheckpointDescriptor,
    actual_backing_blocks: u64,
    segment_capacity: u32,
    buffers: &mut [AlignedBlock],
) -> io::Result<Loaded> {
    let geometry = descriptor.geometry()?;
    if actual_backing_blocks < descriptor.backing_blocks {
        return Err(invalid_data(
            "backing is shorter than the recovered provisioned bound",
        ));
    }
    let mut index = MappingIndex::new(geometry)?;
    let mut segments = Segments::new(geometry, segment_capacity)?;
    load_index(io, backing, descriptor, &mut index, buffers)?;
    load_table(io, backing, descriptor, &mut segments, buffers)?;
    segments.load_table_blocks(&descriptor.table_pages[..geometry.table_pages as usize])?;
    let heads = load_heads(descriptor, geometry, &segments)?;
    for head in heads {
        segments.record_used(
            head.segment,
            head.next_block - geometry.segment_start(head.segment)?,
        );
    }
    let live_blocks = validate_snapshots(io, backing, descriptor, &index, &mut segments, buffers)?;
    for record in index.record_blocks() {
        segments.add_metadata(record)?;
    }
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(BATCH_BLOCKS)
        .map_err(|_| io::Error::other("scratch allocation failed"))?;
    scratch.resize_with(BATCH_BLOCKS, || AlignedBlock([0; BLOCK_SIZE]));
    let scratch = scratch.into_boxed_slice();
    Ok(Loaded {
        #[cfg(test)]
        descriptor: descriptor.clone(),
        log: Log {
            volume_id: descriptor.volume_id,
            geometry,
            heads,
            segments,
            index,
            next_lsn: descriptor
                .checkpoint_lsn
                .checked_add(1)
                .ok_or_else(|| invalid_data("checkpoint LSN exhausted"))?,
            active_slot: descriptor.slot,
            root_lsn: [None, None],
            blocks_since_checkpoint: 0,
            writes: LogWrites::default(),
            scratch,
        },
        live_blocks,
    })
}

/// Loads the indirect, then the direct index pages: direct pointers past the descriptor's
/// array come from the indirect pages.
fn load_index(
    io: &mut BackingIo,
    backing: &File,
    descriptor: &CheckpointDescriptor,
    index: &mut MappingIndex,
    buffers: &mut [AlignedBlock],
) -> io::Result<()> {
    let geometry = descriptor.geometry()?;
    let expectation = |page_id, level, boundary| IndexPageExpectation {
        geometry,
        volume_id: descriptor.volume_id,
        page_id,
        level,
        boundary,
        maximum_lsn: descriptor.checkpoint_lsn,
    };
    let indirect =
        (0..geometry.indirect_pages).map(|page| (page, descriptor.indirect_pages[page as usize]));
    read_pages(io, backing, buffers, indirect, |page, block, bytes| {
        let expected = expectation(page, MappingIndexLevel::Indirect, block);
        index.load_indirect(page, block, &MappingIndexPage::decode(bytes, &expected)?)
    })?;
    let mut direct = Vec::new();
    direct
        .try_reserve_exact(geometry.direct_pages as usize)
        .map_err(|_| io::Error::other("index pointer allocation failed"))?;
    direct.extend((0..geometry.direct_pages).map(|page| {
        let block = match descriptor.direct_pages.get(page as usize) {
            Some(&block) => block,
            None => index.direct_block(page),
        };
        (page, block)
    }));
    read_pages(io, backing, buffers, direct, |page, block, bytes| {
        let expected = expectation(page, MappingIndexLevel::Direct, block);
        index.load_direct(page, block, &MappingIndexPage::decode(bytes, &expected)?)
    })
}

fn load_table(
    io: &mut BackingIo,
    backing: &File,
    descriptor: &CheckpointDescriptor,
    segments: &mut Segments,
    buffers: &mut [AlignedBlock],
) -> io::Result<()> {
    let geometry = descriptor.geometry()?;
    let tables =
        (0..geometry.table_pages).map(|table| (table, descriptor.table_pages[table as usize]));
    read_pages(io, backing, buffers, tables, |table, block, bytes| {
        let expected = TablePageExpectation {
            geometry,
            volume_id: descriptor.volume_id,
            page_id: table,
            boundary: block,
            maximum_lsn: descriptor.checkpoint_lsn,
        };
        let page = SegmentTablePage::decode(bytes, &expected)?;
        let first = table * TABLE_ENTRIES as u32;
        for slot in 0..geometry.table_children(table) {
            let entry = page.entry(slot)?;
            if entry.state != SegmentState::Free {
                segments.load_entry(first + slot as u32, entry)?;
            }
        }
        Ok(())
    })
}

/// Each head owns exactly one open segment. The head resumes after its boundary when that
/// boundary lies in the open segment, else at the segment start (the boundary is a link).
fn load_heads(
    descriptor: &CheckpointDescriptor,
    geometry: Geometry,
    segments: &Segments,
) -> io::Result<[Head; 2]> {
    let mut heads = [None, None];
    for segment in 0..geometry.segment_count {
        let entry = segments.entry(segment)?;
        let Some(head) = entry.head else { continue };
        if heads[head.index()].is_some() {
            return Err(invalid_data("a head owns two open segments"));
        }
        heads[head.index()] = Some(segment);
    }
    let mut cursors = [Head::opened(HeadId::User, 0, geometry)?; 2];
    for id in HeadId::ALL {
        let segment =
            heads[id.index()].ok_or_else(|| invalid_data("a head has no open segment"))?;
        let boundary = descriptor.head_boundary(id);
        let mut head = Head::opened(id, segment, geometry)?;
        head.last_boundary = boundary;
        head.last_lsn = descriptor.checkpoint_lsn;
        if boundary != 0 && geometry.segment_of(boundary)? == segment {
            head.next_block = boundary + 1;
            if u64::from(head.next_block) >= geometry.segment_end(segment)? {
                return Err(invalid_data("head boundary fills its open segment"));
            }
        }
        cursors[id.index()] = head;
    }
    Ok(cursors)
}

/// Reads every snapshot the root references, validates it and rebuilds live counts.
fn validate_snapshots(
    io: &mut BackingIo,
    backing: &File,
    descriptor: &CheckpointDescriptor,
    index: &MappingIndex,
    segments: &mut Segments,
    buffers: &mut [AlignedBlock],
) -> io::Result<u64> {
    let geometry = descriptor.geometry()?;
    let mut live_blocks = 0;
    let snapshots = (0..geometry.page_count).map(|page| (page, index.snapshot_block(page)));
    read_pages(
        io,
        backing,
        buffers,
        snapshots,
        |page_id, boundary, bytes| {
            let page = MappingPageRecord::decode(
                bytes,
                &PageExpectation {
                    geometry,
                    volume_id: descriptor.volume_id,
                    page_id,
                    boundary,
                    maximum_lsn: descriptor.checkpoint_lsn,
                },
            )?;
            for &address in page.entries.physical_addr.iter().filter(|&&a| a != 0) {
                segments.add_live(address)?;
                live_blocks += 1;
            }
            Ok(())
        },
    )?;
    Ok(live_blocks)
}
