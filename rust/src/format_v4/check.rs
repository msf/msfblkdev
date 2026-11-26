//! ADR-11 offline check: every checksum and reference in every non-free segment.
//!
//! This is the structural checker that ADR-05 asks for. It shares only the record codecs and
//! `Geometry` with the engine. It doesn't call recovery, `Segments`, `MappingIndex` or the
//! cleaner, so it doesn't trust the code it checks. It never writes, and it holds a shared
//! claim on the backing, which keeps `open_live` out while it runs.
use super::{
    Geometry, HeadId, NO_PREVIOUS_BOUNDARY, RecordHeader, RecordKind, VERSION,
    data_record::DataRecord,
    decode_metadata_framing, decode_metadata_record,
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    geometry::{DIRECT_POINTERS, INDEX_CHILDREN, TABLE_ENTRIES},
    index_page::{IndexPageExpectation, MappingIndexLevel, MappingIndexPage},
    looks_like_record,
    mapping_page::{MappingPageRecord, PageExpectation},
    readonly::TailSummary,
    segment_table::{SegmentEntry, SegmentState, SegmentTablePage, TablePageExpectation},
    small_records::{BackingGrowth, LbaDiscard, SegmentLink},
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS,
    backing::{self, OpenMode, Opened},
    mapping_cache::ENTRIES_PER_PAGE,
    payload_checksum, read_u32,
};
use std::{fmt, fs::File, io, os::unix::fs::FileExt, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Damage that reads, replay or a later open can hit.
    Error,
    /// Damage that no read can return, such as a dead payload with a bad checksum.
    Warning,
}

/// One problem `check` found. Each location field is set where it applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub segment: Option<u32>,
    pub block: Option<u64>,
    pub lsn: Option<u64>,
    pub lba: Option<u32>,
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(formatter, "{severity:<6}")?;
        if let Some(segment) = self.segment {
            write!(formatter, " segment {segment}")?;
        }
        if let Some(block) = self.block {
            write!(formatter, " block {block}")?;
        }
        if let Some(lsn) = self.lsn {
            write!(formatter, " LSN {lsn}")?;
        }
        if let Some(lba) = self.lba {
            write!(formatter, " LBA {lba}")?;
        }
        let located = self.segment.is_some() || self.block.is_some();
        let separator = if located || self.lsn.is_some() || self.lba.is_some() {
            ":"
        } else {
            ""
        };
        write!(formatter, "{separator} {}", self.message)
    }
}

/// The outcome of a check that ran to the end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub errors: u64,
    pub warnings: u64,
    /// The newest root whose descriptor decodes; `None` when neither does.
    pub selected: Option<CheckpointSlot>,
    pub checkpoint_lsn: u64,
    /// Non-free segments whose record chains were walked.
    pub segments_walked: u32,
    /// Records found on the walked chains, the tail excluded.
    pub records: u64,
    /// Payload blocks whose checksums were verified, the tail included.
    pub payloads: u64,
    /// Nonzero mapping entries of the selected root.
    pub mapped_blocks: u64,
    /// Records after the checkpoint that the next `open_live` replays.
    pub tail: [TailSummary; 2],
}

impl CheckReport {
    #[must_use]
    pub fn has_tail(&self) -> bool {
        self.tail.iter().any(|tail| tail.records != 0)
    }
}

/// Verifies every checksum and reference in every non-free segment of an offline backing.
///
/// Calls `on_finding` once per problem, in the order found. Returns `Err` only when the check
/// could not run: an I/O error, a backing that isn't format 4, a backing that `open_live` owns
/// (`WouldBlock` for a file, `ResourceBusy` for a device), or too little memory for the mapping.
pub fn check(
    backing_path: impl AsRef<Path>,
    on_finding: &mut dyn FnMut(&Finding),
) -> io::Result<CheckReport> {
    let Opened { file, kind, claim } = backing::open(backing_path.as_ref(), OpenMode::Check)?;
    let bytes = backing::extent(&file, kind)?.bytes;
    if !bytes.is_multiple_of(BLOCK_SIZE as u64) || bytes < 2 * BLOCK_SIZE as u64 {
        return Err(not_format4(
            "the size is not a whole number of 4 KiB blocks",
        ));
    }
    let mut checker = Checker {
        file,
        file_blocks: bytes / BLOCK_SIZE as u64,
        block: AlignedBlock([0; BLOCK_SIZE]),
        scan: AlignedBlock([0; BLOCK_SIZE]),
        report: CheckReport::default(),
        sink: on_finding,
    };
    let roots = checker.read_roots()?;
    let mut order = CheckpointSlot::BOTH;
    if lsn(&roots[1]) > lsn(&roots[0]) {
        order.swap(0, 1);
    }
    let [newest, older] = order.map(|slot| roots[slot.index()].clone());
    if let Some(descriptor) = older {
        checker.walk_root(&descriptor, false)?;
    }
    if let Some(descriptor) = newest {
        checker.report.selected = Some(descriptor.slot);
        checker.report.checkpoint_lsn = descriptor.checkpoint_lsn;
        let graph = checker.walk_root(&descriptor, true)?;
        checker.check_segments(graph)?;
    }
    drop(claim);
    Ok(checker.report)
}

fn not_format4(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("not a format-4 backing: {reason}"),
    )
}

fn lsn(root: &Option<CheckpointDescriptor>) -> Option<u64> {
    root.as_ref().map(|descriptor| descriptor.checkpoint_lsn)
}

fn out_of_memory() -> io::Error {
    io::Error::new(
        io::ErrorKind::OutOfMemory,
        "not enough memory to hold the volume's references",
    )
}

/// A vector of `length` copies of `value`, or an error instead of an abort.
fn filled<T: Clone>(length: usize, value: T) -> io::Result<Vec<T>> {
    let mut vector = Vec::new();
    vector
        .try_reserve_exact(length)
        .map_err(|_| out_of_memory())?;
    vector.resize(length, value);
    Ok(vector)
}

/// Where a finding applies.
#[derive(Clone, Copy, Default)]
struct Place {
    segment: Option<u32>,
    block: Option<u64>,
    lsn: Option<u64>,
    lba: Option<u32>,
}

impl Place {
    fn block(geometry: Geometry, block: u32) -> Self {
        Self {
            segment: geometry.segment_of(block).ok(),
            block: Some(u64::from(block)),
            ..Self::default()
        }
    }

    fn descriptor(slot: CheckpointSlot) -> Self {
        Self {
            block: Some(u64::from(slot.block())),
            ..Self::default()
        }
    }

    fn lsn(self, lsn: u64) -> Self {
        Self {
            lsn: Some(lsn),
            ..self
        }
    }

    fn lba(self, lba: u32) -> Self {
        Self {
            lba: Some(lba),
            ..self
        }
    }
}

/// One nonzero mapping entry of the selected root.
#[derive(Clone, Copy)]
struct Mapped {
    physical: u32,
    lba: u32,
    checksum: u64,
}

/// What one root references. Entries that failed to decode are left out and reported.
struct RootGraph {
    descriptor: CheckpointDescriptor,
    geometry: Geometry,
    /// One entry per segment; `None` where a table page failed to decode.
    table: Vec<Option<SegmentEntry>>,
    /// Blocks of every index, mapping and table page that decoded, sorted.
    metadata: Vec<u32>,
    /// Blocks of the referenced pages that failed to decode; each is already reported.
    failed_pages: Vec<u32>,
    /// Selected root only: every nonzero mapping entry, sorted by physical block.
    mapping: Vec<Mapped>,
}

/// The selected root's references and which of them the segment walks found.
struct References {
    mapping: Vec<Mapped>,
    mapping_found: Vec<bool>,
    metadata: Vec<u32>,
    metadata_found: Vec<bool>,
    failed_pages: Vec<u32>,
    /// Per segment, the block where its chain broke; nothing at or after it is verified.
    chain_break: Vec<Option<u32>>,
}

impl References {
    fn new(graph: &mut RootGraph) -> io::Result<Self> {
        let mapping = std::mem::take(&mut graph.mapping);
        let metadata = std::mem::take(&mut graph.metadata);
        Ok(Self {
            failed_pages: std::mem::take(&mut graph.failed_pages),
            chain_break: filled(graph.geometry.segment_count as usize, None)?,
            mapping_found: filled(mapping.len(), false)?,
            metadata_found: filled(metadata.len(), false)?,
            mapping,
            metadata,
        })
    }

    fn mapped(&self, physical: u32) -> Option<usize> {
        self.mapping
            .binary_search_by_key(&physical, |entry| entry.physical)
            .ok()
    }

    /// Whether the walk could verify `block`: its segment's chain didn't break before it.
    fn verifiable(&self, geometry: Geometry, block: u32) -> bool {
        let segment = geometry
            .segment_of(block)
            .map_or(0, |segment| segment as usize);
        self.chain_break[segment].is_none_or(|broken| block < broken)
    }

    fn page_failed(&self, block: u32) -> bool {
        self.failed_pages.contains(&block)
    }

    fn metadata_on_chain(&mut self, block: u32) {
        if let Ok(index) = self.metadata.binary_search(&block) {
            self.metadata_found[index] = true;
        }
    }
}

/// The append position of one head, from the root.
#[derive(Clone, Copy)]
struct Cursor {
    head: HeadId,
    segment: u32,
    next_block: u32,
    last_boundary: u32,
    last_lsn: u64,
}

/// The block where a segment's record chain must end.
#[derive(Clone, Copy)]
enum ChainEnd {
    /// An open segment ends at its head boundary.
    Boundary(u32),
    /// A sealed segment ends with a `SegmentLink` that carries its seal LSN.
    Link { seal_lsn: u64 },
}

/// Why no record follows a position.
struct Damage {
    /// A block that looks like this volume's metadata failed validation; replay fails on it.
    metadata: bool,
    reason: String,
}

enum Found {
    Data(Box<DataRecord>),
    /// A record whose framing chains. `body` holds the error when the full block fails;
    /// replay still follows such a record, so the walk continues past it.
    Metadata {
        header: RecordHeader,
        block: Box<AlignedBlock>,
        body: Option<String>,
    },
    Nothing(Option<Damage>),
}

struct Checker<'a> {
    file: File,
    file_blocks: u64,
    block: AlignedBlock,
    scan: AlignedBlock,
    report: CheckReport,
    sink: &'a mut dyn FnMut(&Finding),
}

impl Checker<'_> {
    fn read(&mut self, block: u64) -> io::Result<()> {
        self.file
            .read_exact_at(&mut self.block.0, block * BLOCK_SIZE as u64)
    }

    fn read_scan(&mut self, block: u64) -> io::Result<()> {
        self.file
            .read_exact_at(&mut self.scan.0, block * BLOCK_SIZE as u64)
    }

    fn found(&mut self, severity: Severity, place: Place, message: String) {
        match severity {
            Severity::Error => self.report.errors += 1,
            Severity::Warning => self.report.warnings += 1,
        }
        (self.sink)(&Finding {
            severity,
            segment: place.segment,
            block: place.block,
            lsn: place.lsn,
            lba: place.lba,
            message,
        });
    }

    fn error(&mut self, place: Place, message: String) {
        self.found(Severity::Error, place, message);
    }

    /// Step 1: both descriptors decode, and a valid pair agrees on identity and geometry.
    fn read_roots(&mut self) -> io::Result<[Option<CheckpointDescriptor>; 2]> {
        let mut decoded = [None, None, None, None];
        let mut format4 = false;
        for slot in CheckpointSlot::BOTH {
            self.read(u64::from(slot.block()))?;
            format4 |= self.block.0[..4] == *b"VBLC" && self.block.0[4] == VERSION;
            decoded[slot.index()] = Some(CheckpointDescriptor::decode(&self.block.0, slot));
        }
        if !format4 {
            return Err(not_format4("neither descriptor has the format-4 magic"));
        }
        let mut roots = [None, None];
        for slot in CheckpointSlot::BOTH {
            match decoded[slot.index()].take().expect("both slots were read") {
                Ok(descriptor) => roots[slot.index()] = Some(descriptor),
                Err(error) => self.error(
                    Place::descriptor(slot),
                    format!("{} descriptor: {error}", slot.name()),
                ),
            }
        }
        if let [Some(green), Some(blue)] = &roots
            && let Err(error) = green.validate_pair(blue)
        {
            self.error(Place::descriptor(CheckpointSlot::Green), error.to_string());
        }
        Ok(roots)
    }

    /// Step 2: every index, mapping and table page of a root decodes and lies in a segment
    /// that the root's table marks non-free.
    fn walk_root(
        &mut self,
        descriptor: &CheckpointDescriptor,
        selected: bool,
    ) -> io::Result<RootGraph> {
        let geometry = descriptor.geometry()?;
        let mut graph = RootGraph {
            descriptor: descriptor.clone(),
            geometry,
            table: filled(geometry.segment_count as usize, None)?,
            metadata: Vec::new(),
            failed_pages: Vec::new(),
            mapping: Vec::new(),
        };
        self.read_table(&mut graph)?;
        let snapshots = self.read_index(&mut graph)?;
        self.read_mapping(&mut graph, &snapshots, selected)?;
        for index in 0..graph.metadata.len() {
            let block = graph.metadata[index];
            let segment = geometry.segment_of(block)?;
            if graph.table[segment as usize].is_some_and(|entry| entry.state == SegmentState::Free)
            {
                let message = format!(
                    "{} root: a page it references lies in a free segment",
                    descriptor.slot.name()
                );
                self.error(Place::block(geometry, block), message);
            }
        }
        graph.metadata.sort_unstable();
        graph.mapping.sort_unstable_by_key(|entry| entry.physical);
        Ok(graph)
    }

    fn page_error(&mut self, graph: &mut RootGraph, block: u32, page: &str, error: &io::Error) {
        graph.failed_pages.push(block);
        let message = format!("{} root: {page}: {error}", graph.descriptor.slot.name());
        self.error(Place::block(graph.geometry, block), message);
    }

    fn read_table(&mut self, graph: &mut RootGraph) -> io::Result<()> {
        let geometry = graph.geometry;
        for table in 0..geometry.table_pages {
            let pointer = graph.descriptor.table_pages[table as usize];
            if pointer == 0 {
                continue;
            }
            self.read(u64::from(pointer))?;
            let expectation = TablePageExpectation {
                geometry,
                volume_id: graph.descriptor.volume_id,
                page_id: table,
                boundary: pointer,
                maximum_lsn: graph.descriptor.checkpoint_lsn,
            };
            let page = match SegmentTablePage::decode(&self.block.0, &expectation) {
                Ok(page) => page,
                Err(error) => {
                    let name = format!("segment table page {table}");
                    self.page_error(graph, pointer, &name, &error);
                    continue;
                }
            };
            graph.metadata.push(pointer);
            let first = table as usize * TABLE_ENTRIES;
            for slot in 0..geometry.table_children(table) {
                graph.table[first + slot] = Some(page.entry(slot)?);
            }
        }
        Ok(())
    }

    fn read_index_page(
        &mut self,
        graph: &mut RootGraph,
        level: MappingIndexLevel,
        page_id: u32,
        pointer: u32,
    ) -> io::Result<Option<MappingIndexPage>> {
        self.read(u64::from(pointer))?;
        let expectation = IndexPageExpectation {
            geometry: graph.geometry,
            volume_id: graph.descriptor.volume_id,
            page_id,
            level,
            boundary: pointer,
            maximum_lsn: graph.descriptor.checkpoint_lsn,
        };
        match MappingIndexPage::decode(&self.block.0, &expectation) {
            Ok(page) => {
                graph.metadata.push(pointer);
                Ok(Some(page))
            }
            Err(error) => {
                let name = format!("{level:?} index page {page_id}").to_lowercase();
                self.page_error(graph, pointer, &name, &error);
                Ok(None)
            }
        }
    }

    /// Returns the snapshot block of every mapping page; zero where none is referenced.
    fn read_index(&mut self, graph: &mut RootGraph) -> io::Result<Vec<u32>> {
        let geometry = graph.geometry;
        let mut direct_blocks = filled(geometry.direct_pages as usize, 0)?;
        let described = direct_blocks.len().min(DIRECT_POINTERS);
        direct_blocks[..described].copy_from_slice(&graph.descriptor.direct_pages[..described]);
        for indirect in 0..geometry.indirect_pages {
            let pointer = graph.descriptor.indirect_pages[indirect as usize];
            if pointer == 0 {
                continue;
            }
            let level = MappingIndexLevel::Indirect;
            if let Some(page) = self.read_index_page(graph, level, indirect, pointer)? {
                let first = DIRECT_POINTERS + indirect as usize * INDEX_CHILDREN;
                let children = geometry.indirect_children(indirect);
                direct_blocks[first..first + children]
                    .copy_from_slice(&page.child_blocks[..children]);
            }
        }
        let mut snapshots = filled(geometry.page_count as usize, 0)?;
        for (direct, &pointer) in (0..).zip(&direct_blocks) {
            if pointer == 0 {
                continue;
            }
            let level = MappingIndexLevel::Direct;
            if let Some(page) = self.read_index_page(graph, level, direct, pointer)? {
                let first = direct as usize * INDEX_CHILDREN;
                let children = geometry.direct_children(direct);
                snapshots[first..first + children].copy_from_slice(&page.child_blocks[..children]);
            }
        }
        Ok(snapshots)
    }

    fn read_mapping(
        &mut self,
        graph: &mut RootGraph,
        snapshots: &[u32],
        collect: bool,
    ) -> io::Result<()> {
        let geometry = graph.geometry;
        for (page_id, &pointer) in (0..).zip(snapshots) {
            if pointer == 0 {
                continue;
            }
            self.read(u64::from(pointer))?;
            let expectation = PageExpectation {
                geometry,
                volume_id: graph.descriptor.volume_id,
                page_id,
                boundary: pointer,
                maximum_lsn: graph.descriptor.checkpoint_lsn,
            };
            let page = match MappingPageRecord::decode(&self.block.0, &expectation) {
                Ok(page) => page,
                Err(error) => {
                    let name = format!("mapping page {page_id}");
                    self.page_error(graph, pointer, &name, &error);
                    continue;
                }
            };
            graph.metadata.push(pointer);
            if !collect {
                continue;
            }
            let entries = geometry.page_entries(page_id)?;
            graph
                .mapping
                .try_reserve(entries)
                .map_err(|_| out_of_memory())?;
            let first_lba = page_id * ENTRIES_PER_PAGE as u32;
            for (lba, index) in (first_lba..).zip(0..entries) {
                let physical = page.entries.physical_addr[index];
                if physical != 0 {
                    graph.mapping.push(Mapped {
                        physical,
                        lba,
                        checksum: page.entries.payload_checksum[index],
                    });
                }
            }
        }
        Ok(())
    }
}

impl Checker<'_> {
    /// Steps 3 to 7 on the selected root.
    fn check_segments(&mut self, mut graph: RootGraph) -> io::Result<()> {
        self.report.mapped_blocks = graph.mapping.len() as u64;
        self.check_duplicates(&graph);
        let mut references = References::new(&mut graph)?;
        let cursors = self.head_cursors(&graph);
        for segment in 0..graph.geometry.segment_count {
            let Some(entry) = graph.table[segment as usize] else {
                // Its table page failed; that error covers whatever the segment holds.
                references.chain_break[segment as usize] =
                    Some(graph.geometry.segment_start(segment)?);
                continue;
            };
            let end = match (entry.state, entry.head) {
                (SegmentState::Free, _) => continue,
                (SegmentState::Open, Some(head)) => {
                    let Some(cursors) = &cursors else {
                        // Without a head position the walk has no end; that error is reported.
                        references.chain_break[segment as usize] =
                            Some(graph.geometry.segment_start(segment)?);
                        continue;
                    };
                    let cursor = cursors[head.index()];
                    if cursor.next_block == graph.geometry.segment_start(segment)? {
                        continue;
                    }
                    ChainEnd::Boundary(cursor.last_boundary)
                }
                (SegmentState::Open, None) => unreachable!("an open entry names its head"),
                (SegmentState::Sealed | SegmentState::PendingFree, _) => ChainEnd::Link {
                    seal_lsn: entry.seal_lsn,
                },
            };
            self.report.segments_walked += 1;
            self.walk_segment(&graph, &mut references, segment, end)?;
        }
        self.check_unfound(&graph, &references);
        self.check_accounting(&graph, &references, cursors.as_ref())?;
        if let Some(cursors) = cursors {
            self.report.tail = self.check_tail(&graph, cursors)?;
        }
        Ok(())
    }

    /// Step 5: no physical block is mapped twice.
    fn check_duplicates(&mut self, graph: &RootGraph) {
        let geometry = graph.geometry;
        for index in 1..graph.mapping.len() {
            let (previous, entry) = (graph.mapping[index - 1], graph.mapping[index]);
            if previous.physical == entry.physical {
                let place = Place::block(geometry, entry.physical).lba(entry.lba);
                let message = format!("block is also mapped by LBA {}", previous.lba);
                self.error(place, message);
            }
        }
    }

    /// Each head owns exactly one open segment; its cursor resumes after its boundary.
    fn head_cursors(&mut self, graph: &RootGraph) -> Option<[Cursor; 2]> {
        let geometry = graph.geometry;
        let mut cursors = [None, None];
        for head in HeadId::ALL {
            let mut owned = (0..geometry.segment_count).filter(|&segment| {
                graph.table[segment as usize].is_some_and(|entry| entry.head == Some(head))
            });
            let (Some(segment), None) = (owned.next(), owned.next()) else {
                // A failed table page already explains a missing head.
                if graph.table.iter().all(Option::is_some) {
                    let message = format!(
                        "the root's table has no single open {} segment",
                        head.name()
                    );
                    self.error(Place::default(), message);
                }
                continue;
            };
            let boundary = graph.descriptor.head_boundary(head);
            let start = geometry.segment_start(segment).ok()?;
            let resumes = boundary != 0 && geometry.segment_of(boundary).ok() == Some(segment);
            let next_block = if resumes { boundary + 1 } else { start };
            if u64::from(next_block) >= geometry.segment_end(segment).ok()? {
                let message = format!("{} head boundary fills its open segment", head.name());
                self.error(Place::block(geometry, boundary), message);
                continue;
            }
            cursors[head.index()] = Some(Cursor {
                head,
                segment,
                next_block,
                last_boundary: boundary,
                last_lsn: graph.descriptor.checkpoint_lsn,
            });
        }
        Some([cursors[0]?, cursors[1]?])
    }
}

impl Checker<'_> {
    /// Step 3: the chain from the segment start has valid framing and checksums, strictly
    /// increasing LSNs and matching `previous_boundary` links, and ends where it must.
    fn walk_segment(
        &mut self,
        graph: &RootGraph,
        references: &mut References,
        segment: u32,
        end: ChainEnd,
    ) -> io::Result<()> {
        let geometry = graph.geometry;
        let segment_end = geometry.segment_end(segment)?;
        let mut position = geometry.segment_start(segment)?;
        let mut previous = None;
        // Bounded by the segment: every record moves `position` forward by at least one block.
        while u64::from(position) < segment_end {
            let volume_id = graph.descriptor.volume_id;
            let header = match self.next_record(geometry, volume_id, segment, position, previous)? {
                Found::Data(record) => {
                    self.check_payloads(geometry, volume_id, Some(&mut *references), &record)?;
                    record.header
                }
                Found::Metadata {
                    header,
                    block,
                    body,
                } => {
                    references.metadata_on_chain(header.boundary);
                    let place = Place::block(geometry, position).lsn(header.lsn);
                    let link = (header.kind == RecordKind::SegmentLink)
                        .then(|| SegmentLink::decode(&block.0, geometry, header.volume_id));
                    // The root walk already reported a page it references that failed.
                    if let Some(body) = body
                        && !references.page_failed(header.boundary)
                    {
                        self.error(place, format!("damaged record: {body}"));
                    } else if let Some(Err(error)) = link {
                        self.error(place, format!("damaged SegmentLink: {error}"));
                    }
                    header
                }
                Found::Nothing(damage) => {
                    references.chain_break[segment as usize] = Some(position);
                    let reason =
                        damage.map_or_else(|| "no record follows".to_owned(), |d| d.reason);
                    self.error(
                        Place::block(geometry, position),
                        format!("record chain breaks: {reason}"),
                    );
                    return Ok(());
                }
            };
            self.report.records += 1;
            if self.chain_ends(graph, &header, end) {
                return Ok(());
            }
            previous = Some((header.lsn, header.boundary));
            position = header.boundary + 1;
        }
        let message = match end {
            ChainEnd::Boundary(boundary) => {
                format!("record chain ends before head boundary {boundary}")
            }
            ChainEnd::Link { .. } => "sealed segment has no SegmentLink".to_owned(),
        };
        self.error(Place::block(geometry, position.saturating_sub(1)), message);
        Ok(())
    }

    /// Whether `header` ends the chain; reports a chain that passes its end.
    fn chain_ends(&mut self, graph: &RootGraph, header: &RecordHeader, end: ChainEnd) -> bool {
        let place = Place::block(graph.geometry, header.boundary).lsn(header.lsn);
        if header.lsn > graph.descriptor.checkpoint_lsn {
            self.error(place, "record is newer than the checkpoint".to_owned());
        }
        let is_link = header.kind == RecordKind::SegmentLink;
        match end {
            ChainEnd::Boundary(boundary) if header.boundary >= boundary || is_link => {
                if header.boundary != boundary || is_link {
                    let message = format!("record chain passes head boundary {boundary}");
                    self.error(place, message);
                }
                true
            }
            ChainEnd::Link { seal_lsn } if is_link => {
                if header.lsn != seal_lsn {
                    self.error(
                        place,
                        format!("SegmentLink LSN differs from seal LSN {seal_lsn}"),
                    );
                }
                true
            }
            ChainEnd::Boundary(_) | ChainEnd::Link { .. } => false,
        }
    }

    /// The record that follows `previous` at `position`, with the replay chaining rules.
    /// `previous` is `None` for the first record of a segment.
    fn next_record(
        &mut self,
        geometry: Geometry,
        volume_id: u64,
        segment: u32,
        position: u32,
        previous: Option<(u64, u32)>,
    ) -> io::Result<Found> {
        let chains = |header: &RecordHeader| match previous {
            Some((lsn, boundary)) => header.chains_from(lsn, boundary),
            None => {
                header.previous_boundary == NO_PREVIOUS_BOUNDARY
                    || geometry.segment_of(header.previous_boundary).ok() != Some(segment)
            }
        };
        let expected_previous = previous.map(|(_, boundary)| boundary);
        let mut damage = None;
        self.read(u64::from(position))?;
        if looks_like_record(&self.block.0, volume_id) && self.block.0[5] != RecordKind::Data as u8
        {
            let full = decode_metadata_record(&self.block.0, geometry, volume_id).map(|_| ());
            // Replay's rules: the framing decides the chain, the body is checked separately.
            match decode_metadata_framing(&self.block.0, geometry, volume_id) {
                Ok(header) if header.boundary == position && chains(&header) => {
                    return Ok(Found::Metadata {
                        header,
                        block: Box::new(AlignedBlock(self.block.0)),
                        body: full.err().map(|error| error.to_string()),
                    });
                }
                // Segment reuse leaves intact older records behind; they end no chain.
                Ok(header)
                    if header.boundary == position
                        && previous.is_some_and(|(lsn, _)| header.lsn < lsn)
                        && full.is_ok() => {}
                Ok(header) => {
                    damage = Some(Damage {
                        metadata: true,
                        reason: format!(
                            "record at block {position} (LSN {}) doesn't continue the chain",
                            header.lsn
                        ),
                    });
                }
                Err(error) => {
                    damage = Some(Damage {
                        metadata: true,
                        reason: format!("damaged record at block {position}: {error}"),
                    });
                }
            }
        }
        // A data footer follows its payloads and never sits in the segment's last block.
        let limit = (u64::from(position) + MAXIMUM_PAYLOAD_BLOCKS as u64)
            .min(geometry.segment_end(segment)?.saturating_sub(2));
        for footer in u64::from(position) + 1..=limit {
            self.read_scan(footer)?;
            let bytes = &self.scan.0;
            if !looks_like_record(bytes, volume_id)
                || bytes[5] != RecordKind::Data as u8
                || u64::from(read_u32(bytes, 28)) != footer
            {
                continue;
            }
            match DataRecord::decode(bytes, geometry, volume_id) {
                Ok(record) if chains(&record.header) => return Ok(Found::Data(Box::new(record))),
                Ok(_) => {}
                Err(error) if expected_previous.is_none_or(|b| read_u32(bytes, 24) == b) => {
                    damage.get_or_insert(Damage {
                        metadata: false,
                        reason: format!("damaged data footer at block {footer}: {error}"),
                    });
                }
                Err(_) => {}
            }
        }
        Ok(Found::Nothing(damage))
    }

    /// Step 4 and 5 for one data record: each payload matches its footer checksum, and a
    /// mapped payload's footer names the entry's LBA and checksum. `references` is `None`
    /// for tail records, which no checkpoint maps yet.
    fn check_payloads(
        &mut self,
        geometry: Geometry,
        volume_id: u64,
        mut references: Option<&mut References>,
        record: &DataRecord,
    ) -> io::Result<()> {
        let first = record.header.start(geometry)?;
        for index in 0..record.header.payload_count(geometry)? {
            let physical = first + index as u32;
            let (lba, footer) = (record.lba_ids[index], record.checksums[index]);
            let place = Place::block(geometry, physical)
                .lsn(record.header.lsn)
                .lba(lba);
            self.read(u64::from(physical))?;
            self.report.payloads += 1;
            let actual = payload_checksum(volume_id, lba, physical, &self.block.0);
            let mapped = references.as_deref_mut().and_then(|refs| {
                let index = refs.mapped(physical)?;
                refs.mapping_found[index] = true;
                Some(refs.mapping[index])
            });
            if let Some(entry) = mapped
                && (entry.lba != lba || entry.checksum != footer)
            {
                let message = format!(
                    "mapping entry of LBA {} (checksum {:016x}) points here; the footer names checksum {footer:016x}",
                    entry.lba, entry.checksum
                );
                self.error(place, message);
            }
            if actual == footer {
                continue;
            }
            let message = format!("payload checksum {actual:016x}, footer {footer:016x}");
            match (mapped, references.is_some()) {
                (None, true) => self.found(Severity::Warning, place, format!("dead {message}")),
                _ => self.error(place, message),
            }
        }
        Ok(())
    }

    /// Step 5: every reference was found on a segment's record chain. References past a
    /// broken chain stay unverified; the break is the error.
    fn check_unfound(&mut self, graph: &RootGraph, references: &References) {
        let geometry = graph.geometry;
        for (entry, found) in references.mapping.iter().zip(&references.mapping_found) {
            if !found && references.verifiable(geometry, entry.physical) {
                let place = Place::block(geometry, entry.physical).lba(entry.lba);
                self.error(
                    place,
                    "mapped block is not a payload on its segment's record chain".to_owned(),
                );
            }
        }
        for (&block, found) in references.metadata.iter().zip(&references.metadata_found) {
            if !found && references.verifiable(geometry, block) {
                let place = Place::block(geometry, block);
                self.error(
                    place,
                    "referenced page is not a record on its segment's chain".to_owned(),
                );
            }
        }
    }

    /// Step 7: in every segment, live + metadata ≤ used.
    fn check_accounting(
        &mut self,
        graph: &RootGraph,
        references: &References,
        cursors: Option<&[Cursor; 2]>,
    ) -> io::Result<()> {
        let geometry = graph.geometry;
        let mut referenced = filled(geometry.segment_count as usize, 0_u64)?;
        let blocks = references.mapping.iter().map(|entry| entry.physical);
        for block in blocks.chain(references.metadata.iter().copied()) {
            referenced[geometry.segment_of(block)? as usize] += 1;
        }
        for (segment, &count) in (0..).zip(&referenced) {
            let Some(entry) = graph.table[segment as usize] else {
                continue;
            };
            let used = match entry.head.zip(cursors) {
                Some((head, cursors)) => {
                    cursors[head.index()].next_block - geometry.segment_start(segment)?
                }
                None if entry.state == SegmentState::Free => 0,
                None => geometry.blocks_per_segment,
            };
            if count > u64::from(used) {
                let place = Place {
                    segment: Some(segment),
                    ..Place::default()
                };
                self.error(
                    place,
                    format!("{count} live and metadata blocks exceed {used} used"),
                );
            }
        }
        Ok(())
    }
}

impl Checker<'_> {
    /// Step 6: the records after the checkpoint, in global LSN order as replay takes them,
    /// validate completely. Returns what the next `open_live` replays.
    fn check_tail(
        &mut self,
        graph: &RootGraph,
        mut cursors: [Cursor; 2],
    ) -> io::Result<[TailSummary; 2]> {
        let mut tail = [TailSummary::default(), TailSummary::default()];
        let mut graph_view = TailGraph {
            geometry: graph.geometry,
            opened: Vec::new(),
        };
        let mut pending: [Option<Found>; 2] = [None, None];
        let first = graph.descriptor.checkpoint_lsn + 1;
        let records = usize::try_from(self.file_blocks).unwrap_or(usize::MAX);
        // Bounded by the backing: every replayed record occupies at least one block.
        for expected in (first..).take(records) {
            for cursor in cursors {
                if pending[cursor.head.index()].is_none() {
                    pending[cursor.head.index()] =
                        Some(self.probe_tail(graph, &graph_view, cursor)?);
                }
            }
            let Some(head) = HeadId::ALL.into_iter().find(|head| {
                matches!(&pending[head.index()], Some(found) if lsn_of(found) == Some(expected))
            }) else {
                break;
            };
            let found = pending[head.index()]
                .take()
                .expect("the chosen head has a record");
            let header =
                self.replay_record(graph, &mut graph_view, &mut cursors[head.index()], found)?;
            let Some(header) = header else { break };
            tail[head.index()].add(header.lsn);
        }
        for (cursor, found) in cursors.iter().zip(pending) {
            if let Some(Found::Nothing(Some(damage))) = found
                && damage.metadata
            {
                let place = Place::block(graph_view.geometry, cursor.next_block);
                let message = format!(
                    "{} tail: {}; the next open_live fails here",
                    cursor.head.name(),
                    damage.reason
                );
                self.error(place, message);
            }
        }
        Ok(tail)
    }

    fn probe_tail(
        &mut self,
        graph: &RootGraph,
        view: &TailGraph,
        cursor: Cursor,
    ) -> io::Result<Found> {
        if u64::from(cursor.next_block) >= view.geometry.segment_end(cursor.segment)? {
            return Ok(Found::Nothing(None));
        }
        let previous = Some((cursor.last_lsn, cursor.last_boundary));
        let volume_id = graph.descriptor.volume_id;
        self.next_record(
            view.geometry,
            volume_id,
            cursor.segment,
            cursor.next_block,
            previous,
        )
    }

    /// Validates one replayed record and moves its head. `None` ends the replay.
    fn replay_record(
        &mut self,
        graph: &RootGraph,
        view: &mut TailGraph,
        cursor: &mut Cursor,
        found: Found,
    ) -> io::Result<Option<RecordHeader>> {
        let geometry = view.geometry;
        let header = match found {
            Found::Data(record) => {
                self.check_payloads(geometry, graph.descriptor.volume_id, None, &record)?;
                record.header
            }
            Found::Metadata {
                header,
                block,
                body,
            } => {
                let place = Place::block(geometry, header.boundary).lsn(header.lsn);
                if let Some(body) = body {
                    self.error(
                        place,
                        format!("{} tail: damaged record: {body}", cursor.head.name()),
                    );
                }
                match self.replay_metadata(graph, view, cursor, header, &block) {
                    Ok(true) => return Ok(Some(header)),
                    Ok(false) => header,
                    Err(error) => {
                        let message = format!(
                            "{} tail: {error}; the next open_live fails here",
                            cursor.head.name()
                        );
                        self.error(place, message);
                        return Ok(None);
                    }
                }
            }
            Found::Nothing(_) => return Ok(None),
        };
        cursor.next_block = header.boundary + 1;
        cursor.last_boundary = header.boundary;
        cursor.last_lsn = header.lsn;
        Ok(Some(header))
    }

    /// Decodes a replayed metadata record's body. Returns true when a link moved the cursor.
    fn replay_metadata(
        &mut self,
        graph: &RootGraph,
        view: &mut TailGraph,
        cursor: &mut Cursor,
        header: RecordHeader,
        block: &AlignedBlock,
    ) -> io::Result<bool> {
        let volume_id = graph.descriptor.volume_id;
        match header.kind {
            RecordKind::SegmentLink => {
                let link = SegmentLink::decode(&block.0, view.geometry, volume_id)?;
                let state = graph
                    .table
                    .get(link.next_segment as usize)
                    .copied()
                    .flatten()
                    .map_or(SegmentState::Free, |entry| entry.state);
                if link.head_id != cursor.head
                    || matches!(state, SegmentState::Open | SegmentState::Sealed)
                    || view.opened.contains(&link.next_segment)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("link opens segment {} that is in use", link.next_segment),
                    ));
                }
                view.opened.push(link.next_segment);
                cursor.segment = link.next_segment;
                cursor.next_block = view.geometry.segment_start(link.next_segment)?;
                cursor.last_boundary = header.boundary;
                cursor.last_lsn = header.lsn;
                Ok(true)
            }
            RecordKind::BackingGrowth => {
                let growth = BackingGrowth::decode(&block.0, view.geometry, volume_id)?;
                if growth.new_backing_blocks > self.file_blocks {
                    return Err(crate::invalid_data("growth beyond the backing's size"));
                }
                view.geometry = view.geometry.with_backing(growth.new_backing_blocks)?;
                Ok(false)
            }
            RecordKind::LbaDiscard => {
                LbaDiscard::decode(&block.0, view.geometry, volume_id).map(|_| false)
            }
            RecordKind::Data
            | RecordKind::MappingPage
            | RecordKind::SegmentTablePage
            | RecordKind::MappingIndexPage => Ok(false),
        }
    }
}

/// The tail's view of the log: geometry after replayed growth, segments the tail opened.
struct TailGraph {
    geometry: Geometry,
    opened: Vec<u32>,
}

fn lsn_of(found: &Found) -> Option<u64> {
    match found {
        Found::Data(record) => Some(record.header.lsn),
        Found::Metadata { header, .. } => Some(header.lsn),
        Found::Nothing(_) => None,
    }
}
