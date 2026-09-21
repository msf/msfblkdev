//! Raw space reclamation: victim selection, bounded victim scans, proposals and release.
//!
//! The cleaner side (`scanner`) reads sealed segments through fixed windows that double as
//! proposal storage, so payload bytes are never copied between arenas. The writer side
//! (`volume/reclaim.rs`) validates every proposal against the live state, appends accepted
//! copies on the Relocation head and frees a victim only through two publications.
use super::{Geometry, budget::CleanerShape};
use crate::{AlignedBlock, BLOCK_SIZE};
use std::io;

pub(crate) mod cleaner;
pub(crate) mod scanner;
pub mod scheduler;
pub(crate) mod select;

/// Size of one `Entry`, fixed by the enum layout and checked by the budget tests.
pub(crate) const ENTRY_BYTES: usize = size_of::<Entry>();

/// A current metadata record found in a victim. The writer re-emits it by marking the page
/// dirty; publication then writes it on the User head and the old record dies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetadataKind {
    Direct,
    Indirect,
    Table,
}

/// One record found in a scan window, referencing its block inside the window arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// A raw payload whose current mapping still named its source block when scanned.
    Payload {
        slot: u32,
        lba: u32,
        source_block: u32,
        checksum: u64,
    },
    /// A mapping snapshot at `source_block`; the writer decides whether it is current.
    Snapshot {
        slot: u32,
        page_id: u32,
        source_block: u32,
    },
    Metadata {
        kind: MetadataKind,
        id: u32,
        source_block: u32,
    },
}

/// A scan window over one victim. Its arena holds the raw blocks that were read; `entries`
/// name the live candidates inside it. Ownership moves to the writer and back.
pub(crate) struct ProposalBuffer {
    pub(crate) victim: u32,
    /// The victim's seal LSN when scanned: the source identity the writer checks at apply.
    pub(crate) seal_lsn: u64,
    /// The first physical block held in `arena[0]`.
    pub(crate) window_start: u32,
    pub(crate) arena: Box<[AlignedBlock]>,
    pub(crate) entries: Box<[Entry]>,
    pub(crate) entry_count: usize,
    /// Payload entry indexes sorted by LBA, filled by the scanner.
    pub(crate) payload_order: Box<[u32]>,
    pub(crate) payload_count: usize,
    /// Whether this window reached the victim's closing link.
    pub(crate) last_window: bool,
}

impl ProposalBuffer {
    pub(crate) fn new(shape: CleanerShape) -> io::Result<Self> {
        let blocks = shape.window_blocks as usize;
        let mut arena = Vec::new();
        arena
            .try_reserve_exact(blocks)
            .map_err(|_| io::Error::other("proposal arena allocation failed"))?;
        arena.resize_with(blocks, || AlignedBlock([0; BLOCK_SIZE]));
        Ok(Self {
            victim: 0,
            seal_lsn: 0,
            window_start: 0,
            arena: arena.into_boxed_slice(),
            entries: super::segments::filled(
                blocks,
                Entry::Metadata {
                    kind: MetadataKind::Table,
                    id: 0,
                    source_block: 0,
                },
            )?,
            entry_count: 0,
            payload_order: super::segments::filled(blocks, 0)?,
            payload_count: 0,
            last_window: false,
        })
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        std::mem::size_of_val(self.arena.as_ref())
            + std::mem::size_of_val(self.entries.as_ref())
            + std::mem::size_of_val(self.payload_order.as_ref())
    }

    pub(crate) fn reset(&mut self, victim: u32, seal_lsn: u64, window_start: u32) {
        self.victim = victim;
        self.seal_lsn = seal_lsn;
        self.window_start = window_start;
        self.entry_count = 0;
        self.payload_count = 0;
        self.last_window = false;
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries[..self.entry_count]
    }

    pub(crate) fn payload(&self, slot: u32) -> &[u8; BLOCK_SIZE] {
        &self.arena[slot as usize].0
    }

    fn push(&mut self, entry: Entry) {
        assert!(
            self.entry_count < self.entries.len(),
            "one entry per window block"
        );
        self.entries[self.entry_count] = entry;
        if let Entry::Payload { .. } = entry {
            self.payload_order[self.payload_count] = self.entry_count as u32;
            self.payload_count += 1;
        }
        self.entry_count += 1;
    }

    /// Payload entries in LBA order, as the writer packs them.
    pub(crate) fn sorted_payloads(&self) -> impl Iterator<Item = Entry> + '_ {
        self.payload_order[..self.payload_count]
            .iter()
            .map(|&index| self.entries[index as usize])
    }

    fn sort_payloads(&mut self) {
        let entries = &self.entries;
        self.payload_order[..self.payload_count].sort_unstable_by_key(|&index| {
            match entries[index as usize] {
                Entry::Payload { lba, .. } => lba,
                _ => unreachable!("payload order names payload entries"),
            }
        });
    }
}

/// The immutable state a scan reads against: a copy of the resident snapshot locations made
/// when a victim is selected. The cleaner owns one; the writer keeps a spare to refresh from.
pub(crate) struct View {
    /// Snapshots newer than this LSN are not part of the view.
    pub(crate) maximum_lsn: u64,
    pub(crate) snapshot_block: Box<[u32]>,
}

impl View {
    pub(crate) fn new(geometry: Geometry) -> io::Result<Self> {
        Ok(Self {
            maximum_lsn: 0,
            snapshot_block: super::segments::filled(geometry.page_count as usize, 0)?,
        })
    }

    pub(crate) fn allocated_bytes_for(geometry: Geometry) -> usize {
        geometry.page_count as usize * size_of::<u32>()
    }
}

/// Counters for reclamation work and progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReclaimStats {
    pub victims_selected: u64,
    pub windows_scanned: u64,
    pub scan_blocks_read: u64,
    pub payloads_proposed: u64,
    pub payloads_copied: u64,
    /// Proposed payloads whose mapping moved before apply.
    pub payloads_stale: u64,
    /// Proposals rejected because the victim was no longer the same sealed incarnation.
    pub proposals_rejected_source: u64,
    pub snapshots_copied: u64,
    pub snapshots_dirtied: u64,
    pub index_pages_dirtied: u64,
    pub table_pages_dirtied: u64,
    pub victims_pending: u64,
    /// Sealed segments released without a scan because writes emptied them.
    pub segments_emptied_by_writes: u64,
    pub segments_freed: u64,
    pub barrier_publications: u64,
    pub discards_issued: u64,
    pub discards_failed: u64,
    /// Live payloads whose bytes failed their checksum in a scan; never relocated.
    pub payloads_corrupt: u64,
    pub pressure_steps: u64,
    /// Pressure steps that blocked on the cleaner's report.
    pub pressure_waits: u64,
    pub enospc_all_live: u64,
}

impl ReclaimStats {
    pub fn summary(&self) -> String {
        format!(
            "reclaim_victims_selected={} reclaim_windows_scanned={} reclaim_scan_blocks_read={} reclaim_payloads_proposed={} reclaim_payloads_copied={} reclaim_payloads_stale={} reclaim_proposals_rejected_source={} reclaim_snapshots_copied={} reclaim_snapshots_dirtied={} reclaim_index_pages_dirtied={} reclaim_table_pages_dirtied={} reclaim_victims_pending={} reclaim_segments_emptied_by_writes={} reclaim_segments_freed={} reclaim_barrier_publications={} reclaim_discards_issued={} reclaim_discards_failed={} reclaim_payloads_corrupt={} reclaim_pressure_steps={} reclaim_pressure_waits={} reclaim_enospc_all_live={}",
            self.victims_selected,
            self.windows_scanned,
            self.scan_blocks_read,
            self.payloads_proposed,
            self.payloads_copied,
            self.payloads_stale,
            self.proposals_rejected_source,
            self.snapshots_copied,
            self.snapshots_dirtied,
            self.index_pages_dirtied,
            self.table_pages_dirtied,
            self.victims_pending,
            self.segments_emptied_by_writes,
            self.segments_freed,
            self.barrier_publications,
            self.discards_issued,
            self.discards_failed,
            self.payloads_corrupt,
            self.pressure_steps,
            self.pressure_waits,
            self.enospc_all_live,
        )
    }
}
