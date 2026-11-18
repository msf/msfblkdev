//! Admission: reserve the append space a buffered group needs before accepting a write.
//!
//! The inherited rule reserves `N + R + D + P` blocks: payloads, packed footers, snapshots of
//! already dirty pages and snapshots of the pages the buffer touches. Format 4 adds one footer
//! for every segment the group may cross; links are excluded because every segment keeps its
//! link slot out of the writable room. Index and table pages, relocation copies and both
//! freeing publications come from the reserve segments instead.
use super::{Geometry, heads::Reserve, log::Log};
use crate::mapping_cache::ENTRIES_PER_PAGE;
use crate::{
    BLOCK_SIZE, MAX_VOLUME_BLOCKS, MAXIMUM_PAYLOAD_BLOCKS, MAXIMUM_WRITE_BUFFER_BYTES,
    invalid_input,
};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reservation {
    pub(crate) payload_blocks: u64,
    pub(crate) data_records: u64,
    pub(crate) pending_pages: u64,
    pub(crate) mapping_snapshots: u64,
    pub(crate) total_blocks: u64,
}

impl Reservation {
    pub(crate) fn admit(
        lbas: &[u32],
        page_count: usize,
        dirty_pages: usize,
        lba: u32,
    ) -> io::Result<Self> {
        if lbas.len() >= (MAXIMUM_WRITE_BUFFER_BYTES / BLOCK_SIZE as u64) as usize {
            return Err(invalid_input(
                "pending writes exceed the write-buffer limit",
            ));
        }
        assert!(
            page_count <= lbas.len(),
            "pending page count cannot exceed payload count"
        );
        assert!(
            !lbas.contains(&lba),
            "coalesced writes need no new reservation"
        );
        let page = lba / ENTRIES_PER_PAGE as u32;
        let additional_page = !lbas
            .iter()
            .any(|earlier| *earlier / ENTRIES_PER_PAGE as u32 == page);
        Self::for_counts(
            lbas.len() + 1,
            page_count + usize::from(additional_page),
            dirty_pages,
        )
    }

    /// Reserves `records` one-block records that touch `pages` distinct pages, such as a discard.
    pub(crate) fn for_metadata(
        records: usize,
        pages: usize,
        dirty_pages: usize,
    ) -> io::Result<Self> {
        Self::for_counts_with_records(0, records as u64, pages, dirty_pages)
    }

    pub(crate) fn for_counts(
        payloads: usize,
        pages: usize,
        dirty_pages: usize,
    ) -> io::Result<Self> {
        Self::for_counts_with_records(
            payloads,
            payloads.div_ceil(MAXIMUM_PAYLOAD_BLOCKS) as u64,
            pages,
            dirty_pages,
        )
    }

    fn for_counts_with_records(
        payloads: usize,
        records: u64,
        pages: usize,
        dirty_pages: usize,
    ) -> io::Result<Self> {
        if dirty_pages as u64 > MAX_VOLUME_BLOCKS.div_ceil(ENTRIES_PER_PAGE as u64) {
            return Err(invalid_input("dirty page count exceeds volume limit"));
        }
        // An initially dirty page may be checkpointed before its pending update: reserve both.
        let mapping_snapshots = (dirty_pages as u64)
            .checked_add(pages as u64)
            .ok_or_else(|| invalid_input("mapping snapshot reservation overflow"))?;
        let total_blocks = (payloads as u64)
            .checked_add(records)
            .and_then(|blocks| blocks.checked_add(mapping_snapshots))
            .ok_or_else(|| invalid_input("append reservation overflow"))?;
        Ok(Self {
            payload_blocks: payloads as u64,
            data_records: records,
            pending_pages: pages as u64,
            mapping_snapshots,
            total_blocks,
        })
    }

    /// Blocks to demand from user space, including one extra footer per possible crossing.
    pub(crate) fn demanded_blocks(&self, geometry: Geometry) -> u64 {
        let writable = u64::from(geometry.blocks_per_segment - 1);
        self.total_blocks + self.total_blocks.div_ceil(writable)
    }
}

/// Fails with `ENOSPC` when the User head plus the free segments above the reserve cannot hold
/// the demanded blocks. Nothing is modified.
pub(crate) fn ensure_user_space(
    log: &Log,
    reserve: Reserve,
    demanded_blocks: u64,
) -> io::Result<()> {
    let geometry = log.geometry;
    let available = super::heads::user_available_blocks(
        geometry,
        log.head(super::HeadId::User).room(geometry),
        log.segments.allocatable_count(),
        reserve,
    );
    if demanded_blocks > available {
        return Err(io::Error::from_raw_os_error(libc::ENOSPC));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn reservation_counts_payloads_footers_snapshots_and_crossings() {
        let geometry = Geometry::new(4096, 2 + 12 * 8, 8).unwrap();
        let reservation = Reservation::admit(&[], 0, 0, 0).unwrap();
        assert_eq!(
            reservation,
            Reservation {
                payload_blocks: 1,
                data_records: 1,
                pending_pages: 1,
                mapping_snapshots: 1,
                total_blocks: 3,
            }
        );
        assert_eq!(reservation.demanded_blocks(geometry), 3 + 1);
        let lbas = [0, 1, 336, 2];
        let reserve = Reservation::admit(&lbas, 2, 2, 3).unwrap();
        assert_eq!(reserve.total_blocks, 5 + 1 + 2 + 2);
        assert_eq!(reserve.demanded_blocks(geometry), 10 + 2);
        assert_eq!(
            Reservation::admit(&lbas, 2, 2, 672).unwrap().total_blocks,
            11
        );
        let same_page: Vec<_> = (0..335).collect();
        assert_eq!(
            Reservation::admit(&same_page, 1, 0, 335)
                .unwrap()
                .total_blocks,
            338
        );
        let big = Geometry::new(4096, 2 + 4 * 1024, 1024).unwrap();
        assert_eq!(
            Reservation::admit(&same_page, 1, 0, 335)
                .unwrap()
                .demanded_blocks(big),
            339
        );
        assert_eq!(
            Reservation::for_metadata(1, 3, 2).unwrap().total_blocks,
            1 + 5
        );
        let full: Vec<_> = (0..4096).collect();
        assert!(Reservation::admit(&full, 13, 0, 4096).is_err());
        assert!(Reservation::admit(&[], 0, usize::MAX, 0).is_err());
    }

    #[test]
    fn reservation_covers_all_small_dirty_sets_and_pressure_schedules() {
        // Enumerate all six-write page sequences over three page IDs; the release visits each
        // page once and snapshots can occur before and between groups.
        for mut sequence in 0_u32..3_u32.pow(6) {
            let mut lbas = Vec::new();
            let mut groups = Vec::new();
            for entry in 0..6 {
                let page = sequence % 3;
                sequence /= 3;
                lbas.push(page * 336 + entry);
                if !groups.contains(&page) {
                    groups.push(page);
                }
            }
            for dirty_mask in 0_u32..8 {
                let initial: BTreeSet<_> = (0..3)
                    .filter(|page| dirty_mask & (1 << page) != 0)
                    .collect();
                let reservation =
                    Reservation::for_counts(lbas.len(), groups.len(), initial.len()).unwrap();
                for capacity in initial.len().max(1)..=3 {
                    for schedule in 0..16 {
                        let mut dirty = initial.clone();
                        let mut snapshots = 0;
                        for (index, page) in groups.iter().enumerate() {
                            if (dirty.len() == capacity && !dirty.contains(page))
                                || schedule & (1 << index) != 0
                            {
                                snapshots += dirty.len();
                                dirty.clear();
                            }
                            dirty.insert(*page);
                        }
                        snapshots += dirty.len();
                        assert!(snapshots as u64 <= reservation.mapping_snapshots);
                        assert!(
                            lbas.len() as u64 + reservation.data_records + snapshots as u64
                                <= reservation.total_blocks
                        );
                    }
                }
            }
        }
    }
}
