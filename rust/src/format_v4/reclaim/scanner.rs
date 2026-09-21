//! Scans one sealed victim through fixed windows and records the candidates it holds.
use super::{Entry, MetadataKind, ProposalBuffer, View};
use crate::format_v4::{
    FIRST_SEGMENT_BLOCK, Geometry, NO_PREVIOUS_BOUNDARY, RecordHeader, RecordKind,
    data_record::DataRecord,
    decode_metadata_framing,
    index_page::MappingIndexLevel,
    looks_like_record,
    mapping_page::{MappingPageRecord, PageExpectation},
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS,
    accounted_io::{BackingIo, submit_exact},
    invalid_data,
    mapping_cache::{ENTRIES_PER_PAGE, MappingEntries},
    payload_checksum, read_block_exact, read_u32, read_u64,
};
use io_uring::{opcode, types};
use std::{fs::File, io, os::fd::AsRawFd};

/// Recently decoded snapshots; sorted releases keep consecutive payloads on one page.
const PAGE_CACHE_SLOTS: usize = 4;
pub(crate) const PAGE_CACHE_BYTES: usize =
    PAGE_CACHE_SLOTS * (size_of::<Option<u32>>() + size_of::<MappingEntries>());

/// Where a scan stands inside its victim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanCursor {
    pub(crate) victim: u32,
    pub(crate) seal_lsn: u64,
    /// The first block of the next record to read.
    pub(crate) next_block: u32,
    last_boundary: u32,
    last_lsn: u64,
    pub(crate) done: bool,
}

impl ScanCursor {
    pub(crate) fn start(geometry: Geometry, victim: u32, seal_lsn: u64) -> io::Result<Self> {
        Ok(Self {
            victim,
            seal_lsn,
            next_block: geometry.segment_start(victim)?,
            last_boundary: NO_PREVIOUS_BOUNDARY,
            last_lsn: 0,
            done: false,
        })
    }

    /// Whether `header` is the next record of this segment's chain.
    fn chains(&self, header: RecordHeader, geometry: Geometry) -> bool {
        let first = self.last_lsn == 0;
        let predecessor_ok = if first {
            header.previous_boundary == NO_PREVIOUS_BOUNDARY
                || header.previous_boundary >= FIRST_SEGMENT_BLOCK
                    && geometry.segment_of(header.previous_boundary).ok() != Some(self.victim)
        } else {
            header.previous_boundary == self.last_boundary
        };
        predecessor_ok && header.lsn > self.last_lsn && header.lsn <= self.seal_lsn
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanReport {
    pub(crate) blocks_read: u64,
    pub(crate) payloads_live: u64,
    pub(crate) payloads_dead: u64,
    /// Live payloads whose bytes fail their footer checksum; never relocated.
    pub(crate) payloads_corrupt: u64,
}

/// What one scan reads against: the backing, its geometry and the writer's view.
pub(crate) struct ScanContext<'a> {
    pub(crate) backing: &'a File,
    pub(crate) geometry: Geometry,
    pub(crate) volume_id: u64,
    pub(crate) view: &'a View,
}

pub(crate) struct Scanner {
    io: BackingIo,
    scratch: AlignedBlock,
    page_cache: [(Option<u32>, MappingEntries); PAGE_CACHE_SLOTS],
    next_cache_slot: usize,
}

impl Scanner {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            io: BackingIo::new(None)?,
            scratch: AlignedBlock([0; BLOCK_SIZE]),
            page_cache: std::array::from_fn(|_| (None, MappingEntries::zeroed())),
            next_cache_slot: 0,
        })
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        // The ring is a kernel resource, reported separately; only heap scratch counts here.
        size_of::<AlignedBlock>() + PAGE_CACHE_BYTES
    }

    /// Reads the next window of the victim into `buffer` and records its live candidates.
    pub(crate) fn scan_window(
        &mut self,
        context: &ScanContext<'_>,
        cursor: &mut ScanCursor,
        buffer: &mut ProposalBuffer,
    ) -> io::Result<ScanReport> {
        assert!(!cursor.done, "scan already finished");
        let ScanContext {
            backing,
            geometry,
            volume_id,
            ..
        } = *context;
        let segment_end = geometry.segment_end(cursor.victim)?;
        let window_start = cursor.next_block;
        let window_end = (u64::from(window_start) + buffer.arena.len() as u64).min(segment_end);
        let blocks = (window_end - u64::from(window_start)) as usize;
        assert!(blocks > 0, "cursor inside the segment");
        buffer.reset(cursor.victim, cursor.seal_lsn, window_start);
        self.read_window(backing, window_start, &mut buffer.arena[..blocks])?;
        for slot in 0..PAGE_CACHE_SLOTS {
            self.page_cache[slot].0 = None;
        }
        let mut report = ScanReport {
            blocks_read: blocks as u64,
            ..ScanReport::default()
        };
        let mut position = window_start;
        // Bounded by the window: each record advances `position` or ends the scan.
        while u64::from(position) < window_end {
            let index = (position - window_start) as usize;
            let block = &buffer.arena[index].0;
            let metadata =
                looks_like_record(block, volume_id) && block[5] != RecordKind::Data as u8;
            let next = if metadata {
                self.scan_metadata(geometry, volume_id, cursor, buffer, position)?
            } else {
                let window = &buffer.arena[..blocks];
                let footer =
                    find_footer(window, window_start, window_end, context, cursor, position)?;
                match footer {
                    Some(record) => {
                        self.collect_payloads(context, buffer, &record, &mut report)?;
                        Some(record.header)
                    }
                    None => None,
                }
            };
            match next {
                Some(header) => {
                    cursor.last_boundary = header.boundary;
                    cursor.last_lsn = header.lsn;
                    position = header.boundary + 1;
                    if header.kind == RecordKind::SegmentLink {
                        cursor.done = true;
                        break;
                    }
                }
                None => {
                    // Either the record straddles the window or the chain ends here.
                    let straddles = u64::from(position) + MAXIMUM_PAYLOAD_BLOCKS as u64 + 1
                        > window_end
                        && window_end < segment_end;
                    if !straddles {
                        cursor.done = true;
                    }
                    break;
                }
            }
        }
        cursor.next_block = position;
        if u64::from(position) >= segment_end {
            cursor.done = true;
        }
        buffer.last_window = cursor.done;
        buffer.sort_payloads();
        Ok(report)
    }

    fn read_window(
        &mut self,
        backing: &File,
        start: u32,
        blocks: &mut [AlignedBlock],
    ) -> io::Result<()> {
        let bytes = blocks.len() * BLOCK_SIZE;
        let read = opcode::Read::new(
            types::Fd(backing.as_raw_fd()),
            blocks.as_mut_ptr().cast::<u8>(),
            bytes as u32,
        )
        .offset(u64::from(start) * BLOCK_SIZE as u64)
        .build();
        submit_exact(&mut self.io, read, u64::from(start), bytes as i32)
    }

    fn scan_metadata(
        &mut self,
        geometry: Geometry,
        volume_id: u64,
        cursor: &ScanCursor,
        buffer: &mut ProposalBuffer,
        position: u32,
    ) -> io::Result<Option<RecordHeader>> {
        let slot = position - buffer.window_start;
        let block = &buffer.arena[slot as usize].0;
        let Ok(header) = decode_metadata_framing(block, geometry, volume_id) else {
            return Ok(None);
        };
        if header.boundary != position || !cursor.chains(header, geometry) {
            return Ok(None);
        }
        let entry = match header.kind {
            RecordKind::SegmentLink => {
                if header.lsn != cursor.seal_lsn {
                    return Err(invalid_data("victim link does not carry its seal LSN"));
                }
                None
            }
            RecordKind::MappingPage => Some(Entry::Snapshot {
                slot,
                page_id: read_u32(block, 32),
                source_block: position,
            }),
            RecordKind::MappingIndexPage => Some(Entry::Metadata {
                kind: match MappingIndexLevel::try_from(block[36])? {
                    MappingIndexLevel::Direct => MetadataKind::Direct,
                    MappingIndexLevel::Indirect => MetadataKind::Indirect,
                },
                id: read_u32(block, 32),
                source_block: position,
            }),
            RecordKind::SegmentTablePage => Some(Entry::Metadata {
                kind: MetadataKind::Table,
                id: read_u32(block, 32),
                source_block: position,
            }),
            RecordKind::BackingGrowth | RecordKind::LbaDiscard => None,
            RecordKind::Data => unreachable!("data records are probed by footer"),
        };
        if let Some(entry) = entry {
            buffer.push(entry);
        }
        Ok(Some(header))
    }

    fn collect_payloads(
        &mut self,
        context: &ScanContext<'_>,
        buffer: &mut ProposalBuffer,
        record: &DataRecord,
        report: &mut ScanReport,
    ) -> io::Result<()> {
        let geometry = context.geometry;
        let count = record.header.payload_count(geometry)?;
        let start = record.header.start(geometry)?;
        for index in 0..count {
            let lba = record.lba_ids[index];
            let physical = start + index as u32;
            let live = self.is_live(context, lba, physical)?;
            if !live {
                report.payloads_dead += 1;
                continue;
            }
            let slot = physical - buffer.window_start;
            let bytes = &buffer.arena[slot as usize].0;
            if payload_checksum(context.volume_id, lba, physical, bytes) != record.checksums[index]
            {
                report.payloads_corrupt += 1;
                continue;
            }
            report.payloads_live += 1;
            buffer.push(Entry::Payload {
                slot,
                lba,
                source_block: physical,
                checksum: record.checksums[index],
            });
        }
        Ok(())
    }

    /// Whether the view maps `lba` to `physical`. An unreadable snapshot counts as live so the
    /// writer, not the scanner, decides against stale state.
    fn is_live(&mut self, context: &ScanContext<'_>, lba: u32, physical: u32) -> io::Result<bool> {
        let ScanContext {
            backing,
            geometry,
            volume_id,
            view,
        } = *context;
        let page_id = lba / ENTRIES_PER_PAGE as u32;
        let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
        let snapshot = view.snapshot_block[page_id as usize];
        if snapshot == 0 {
            return Ok(false);
        }
        if let Some(slot) = self
            .page_cache
            .iter()
            .position(|(id, _)| *id == Some(page_id))
        {
            return Ok(self.page_cache[slot].1.physical_addr[entry] == physical);
        }
        read_block_exact(&mut self.io, backing, snapshot, &mut self.scratch)?;
        let decoded = MappingPageRecord::decode(
            &self.scratch.0,
            &PageExpectation {
                geometry,
                volume_id,
                page_id,
                boundary: snapshot,
                maximum_lsn: view.maximum_lsn,
            },
        );
        let Ok(page) = decoded else {
            return Ok(true);
        };
        let live = page.entries.physical_addr[entry] == physical;
        let slot = self.next_cache_slot;
        self.next_cache_slot = (slot + 1) % PAGE_CACHE_SLOTS;
        self.page_cache[slot] = (Some(page_id), page.entries);
        Ok(live)
    }
}

/// Finds the footer of the data record that starts at `position` inside the window.
fn find_footer(
    window: &[AlignedBlock],
    window_start: u32,
    window_end: u64,
    context: &ScanContext<'_>,
    cursor: &ScanCursor,
    position: u32,
) -> io::Result<Option<DataRecord>> {
    let (geometry, volume_id) = (context.geometry, context.volume_id);
    let limit = (u64::from(position) + MAXIMUM_PAYLOAD_BLOCKS as u64).min(window_end - 1);
    let mut boundary = u64::from(position) + 1;
    while boundary <= limit {
        let candidate = u32::try_from(boundary).expect("segment blocks fit u32");
        let block = &window[(candidate - window_start) as usize].0;
        boundary += 1;
        if !looks_like_record(block, volume_id)
            || block[5] != RecordKind::Data as u8
            || read_u32(block, 28) != candidate
            || read_u64(block, 16) > cursor.seal_lsn
        {
            continue;
        }
        if let Ok(record) = DataRecord::decode(block, geometry, volume_id)
            && cursor.chains(record.header, geometry)
            && record.header.start(geometry)? == position
        {
            return Ok(Some(record));
        }
    }
    Ok(None)
}
