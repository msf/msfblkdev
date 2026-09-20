//! Root selection and complete candidate validation before any format-4 write.
use super::{
    Geometry, HeadId,
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    geometry::{DIRECT_POINTERS, TABLE_ENTRIES},
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
    AlignedBlock, BLOCK_SIZE, MAX_BACKING_BLOCKS, accounted_io::BackingIo, invalid_data,
    read_block_exact,
};
use std::{fs::File, io};

pub(crate) struct Loaded {
    pub(crate) log: Log,
    /// Mapped logical blocks counted while validating every referenced snapshot.
    pub(crate) live_blocks: u64,
    /// The root that was selected.
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
    let mut block = AlignedBlock([0; BLOCK_SIZE]);
    let mut candidates: [Option<CheckpointDescriptor>; 2] = [None, None];
    for (candidate, slot) in candidates.iter_mut().zip(CheckpointSlot::BOTH) {
        read_block_exact(io, backing, slot.block(), &mut block)?;
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
            &mut block,
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

fn load_candidate(
    io: &mut BackingIo,
    backing: &File,
    descriptor: &CheckpointDescriptor,
    actual_backing_blocks: u64,
    segment_capacity: u32,
    block: &mut AlignedBlock,
) -> io::Result<Loaded> {
    let geometry = descriptor.geometry()?;
    if actual_backing_blocks < descriptor.backing_blocks {
        return Err(invalid_data(
            "backing is shorter than the recovered provisioned bound",
        ));
    }
    let volume_id = descriptor.volume_id;
    let maximum_lsn = descriptor.checkpoint_lsn;
    let mut index = MappingIndex::new(geometry)?;
    let mut segments = Segments::new(geometry, segment_capacity)?;
    for indirect in 0..geometry.indirect_pages {
        let pointer = descriptor.indirect_pages[indirect as usize];
        if pointer == 0 {
            continue;
        }
        read_block_exact(io, backing, pointer, block)?;
        io.mapping_io.page_reads += 1;
        let page = MappingIndexPage::decode(
            &block.0,
            &IndexPageExpectation {
                geometry,
                volume_id,
                page_id: indirect,
                level: MappingIndexLevel::Indirect,
                boundary: pointer,
                maximum_lsn,
            },
        )?;
        index.load_indirect(indirect, pointer, &page)?;
    }
    for direct in 0..geometry.direct_pages {
        let pointer = if (direct as usize) < DIRECT_POINTERS {
            descriptor.direct_pages[direct as usize]
        } else {
            index.direct_block(direct)
        };
        if pointer == 0 {
            continue;
        }
        read_block_exact(io, backing, pointer, block)?;
        io.mapping_io.page_reads += 1;
        let page = MappingIndexPage::decode(
            &block.0,
            &IndexPageExpectation {
                geometry,
                volume_id,
                page_id: direct,
                level: MappingIndexLevel::Direct,
                boundary: pointer,
                maximum_lsn,
            },
        )?;
        index.load_direct(direct, pointer, &page)?;
    }
    for table in 0..geometry.table_pages {
        let pointer = descriptor.table_pages[table as usize];
        if pointer == 0 {
            continue;
        }
        read_block_exact(io, backing, pointer, block)?;
        io.mapping_io.page_reads += 1;
        let page = SegmentTablePage::decode(
            &block.0,
            &TablePageExpectation {
                geometry,
                volume_id,
                page_id: table,
                boundary: pointer,
                maximum_lsn,
            },
        )?;
        let first = table * TABLE_ENTRIES as u32;
        for slot in 0..geometry.table_children(table) {
            let entry = page.entry(slot)?;
            if entry.state != SegmentState::Free {
                segments.load_entry(first + slot as u32, entry)?;
            }
        }
    }
    segments.load_table_blocks(&descriptor.table_pages[..geometry.table_pages as usize])?;
    let heads = load_heads(descriptor, geometry, &segments)?;
    for head in heads {
        segments.record_used(
            head.segment,
            head.next_block - geometry.segment_start(head.segment)?,
        );
    }
    let live_blocks = validate_snapshots(io, backing, descriptor, &index, &mut segments, block)?;
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
        descriptor: descriptor.clone(),
        log: Log {
            volume_id,
            geometry,
            heads,
            segments,
            index,
            next_lsn: maximum_lsn
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
    block: &mut AlignedBlock,
) -> io::Result<u64> {
    let geometry = descriptor.geometry()?;
    let mut live_blocks = 0;
    for page_id in 0..geometry.page_count {
        let boundary = index.snapshot_block(page_id);
        if boundary == 0 {
            continue;
        }
        read_block_exact(io, backing, boundary, block)?;
        io.mapping_io.page_reads += 1;
        let page = MappingPageRecord::decode(
            &block.0,
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
    }
    Ok(live_blocks)
}
