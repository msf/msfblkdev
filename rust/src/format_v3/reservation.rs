use crate::mapping_cache::ENTRIES_PER_PAGE;
use crate::{
    BLOCK_SIZE, MAX_BACKING_BLOCKS, MAX_VOLUME_BLOCKS, MAXIMUM_PAYLOAD_BLOCKS,
    MAXIMUM_WRITE_BUFFER_BYTES, invalid_input,
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

    /// Release sorts the whole buffer and checkpoints only before appending it.
    #[cfg(test)]
    pub(crate) fn buffered(
        lbas: &[u32],
        dirty_pages: usize,
        additional: Option<u32>,
    ) -> io::Result<Self> {
        let additional = additional.filter(|lba| !lbas.contains(lba));
        let payloads = lbas
            .len()
            .checked_add(usize::from(additional.is_some()))
            .ok_or_else(|| invalid_input("pending payload count overflow"))?;
        if payloads > (MAXIMUM_WRITE_BUFFER_BYTES / BLOCK_SIZE as u64) as usize
            || dirty_pages as u64 > MAX_VOLUME_BLOCKS.div_ceil(ENTRIES_PER_PAGE as u64)
        {
            return Err(invalid_input(
                "pending writes exceed the write-buffer limit",
            ));
        }
        let mut pages = 0;
        for (index, lba) in lbas.iter().enumerate() {
            let page = *lba / ENTRIES_PER_PAGE as u32;
            if !lbas[..index]
                .iter()
                .any(|earlier| *earlier / ENTRIES_PER_PAGE as u32 == page)
            {
                pages += 1;
            }
        }
        if let Some(lba) = additional {
            let page = lba / ENTRIES_PER_PAGE as u32;
            if !lbas
                .iter()
                .any(|earlier| *earlier / ENTRIES_PER_PAGE as u32 == page)
            {
                pages += 1;
            }
        }
        Self::for_counts(payloads, pages, dirty_pages)
    }

    fn for_counts(payloads: usize, pages: usize, dirty_pages: usize) -> io::Result<Self> {
        if dirty_pages as u64 > MAX_VOLUME_BLOCKS.div_ceil(ENTRIES_PER_PAGE as u64) {
            return Err(invalid_input("dirty page count exceeds volume limit"));
        }
        // An initially dirty page may be checkpointed before its pending update.
        // Reserve its old snapshot and a new snapshot, even when the IDs overlap.
        let mapping_snapshots = (dirty_pages as u64)
            .checked_add(pages as u64)
            .ok_or_else(|| invalid_input("mapping snapshot reservation overflow"))?;
        let data_records = payloads.div_ceil(MAXIMUM_PAYLOAD_BLOCKS) as u64;
        let total_blocks = (payloads as u64)
            .checked_add(data_records)
            .and_then(|blocks| blocks.checked_add(mapping_snapshots))
            .ok_or_else(|| invalid_input("append reservation overflow"))?;
        Ok(Self {
            payload_blocks: payloads as u64,
            data_records,
            pending_pages: pages as u64,
            mapping_snapshots,
            total_blocks,
        })
    }
}

pub(crate) fn ensure_space(boundary: u32, backing_blocks: u64, required: u64) -> io::Result<()> {
    if backing_blocks > MAX_BACKING_BLOCKS || u64::from(boundary) >= backing_blocks {
        return Err(invalid_input("invalid append geometry"));
    }
    let available = backing_blocks - (u64::from(boundary) + 1);
    if required > available {
        return Err(io::Error::from_raw_os_error(libc::ENOSPC));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn reservation_counts_packed_records_and_checkpoint_debt() {
        assert_eq!(Reservation::buffered(&[], 0, None).unwrap().total_blocks, 0);
        assert_eq!(
            Reservation::buffered(&[], 0, Some(0)).unwrap(),
            Reservation {
                payload_blocks: 1,
                data_records: 1,
                pending_pages: 1,
                mapping_snapshots: 1,
                total_blocks: 3,
            }
        );
        let lbas = [0, 1, 320, 2];
        let reserve = Reservation::buffered(&lbas, 2, None).unwrap();
        assert_eq!(
            reserve,
            Reservation {
                payload_blocks: 4,
                data_records: 1,
                pending_pages: 2,
                mapping_snapshots: 4,
                total_blocks: 9
            }
        );
        assert_eq!(Reservation::buffered(&lbas, 2, Some(1)).unwrap(), reserve);
        assert_eq!(
            Reservation::buffered(&lbas, 2, Some(3))
                .unwrap()
                .total_blocks,
            10
        );
        assert_eq!(
            Reservation::buffered(&lbas, 2, Some(640))
                .unwrap()
                .total_blocks,
            11
        );
        let same_page: Vec<_> = (0..320).collect();
        assert_eq!(
            Reservation::buffered(&same_page, 0, None)
                .unwrap()
                .total_blocks,
            322
        );
    }

    #[test]
    fn capacity_errors_precede_append_and_preserve_u32_endpoints() {
        assert!(ensure_space(3, 7, 3).is_ok());
        assert_eq!(
            ensure_space(3, 7, 4).unwrap_err().raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert!(ensure_space(u32::MAX, 1_u64 << 32, 0).is_ok());
        assert_eq!(
            ensure_space(u32::MAX, 1_u64 << 32, 1)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert_eq!(
            ensure_space(3, 7, u64::MAX).unwrap_err().raw_os_error(),
            Some(libc::ENOSPC)
        );
        for (boundary, backing) in [(0, 0), (7, 7), (0, (1_u64 << 32) + 1)] {
            assert_eq!(
                ensure_space(boundary, backing, 0).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let full: Vec<_> = (0..4096).collect();
        assert!(Reservation::buffered(&full, 0, Some(4096)).is_err());
        assert!(Reservation::buffered(&[], usize::MAX, Some(0)).is_err());
        assert!(Reservation::admit(&full, 13, 0, 4096).is_err());
        assert!(Reservation::admit(&[], 0, usize::MAX, 0).is_err());
    }

    #[test]
    fn reservation_covers_all_small_dirty_sets_and_pressure_schedules() {
        // Enumerate all six-write page sequences over three page IDs. The release
        // visits each page once; snapshots can occur before and between groups.
        for mut sequence in 0_u32..3_u32.pow(6) {
            let mut lbas = Vec::new();
            let mut groups = Vec::new();
            for entry in 0..6 {
                let page = sequence % 3;
                sequence /= 3;
                lbas.push(page * 320 + entry);
                if !groups.contains(&page) {
                    groups.push(page);
                }
            }
            for dirty_mask in 0_u32..8 {
                let initial: BTreeSet<_> = (0..3)
                    .filter(|page| dirty_mask & (1 << page) != 0)
                    .collect();
                let reservation = Reservation::buffered(&lbas, initial.len(), None).unwrap();
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
