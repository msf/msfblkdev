use crate::mapping_cache::ENTRIES_PER_PAGE;
use crate::{BLOCK_SIZE, MAXIMUM_WRITE_BUFFER_BYTES, WriteBackBuffer};

pub(crate) struct PageGroup {
    pub(crate) page_id: u32,
    slots: [usize; ENTRIES_PER_PAGE],
    count: usize,
}

impl PageGroup {
    pub(crate) fn slots(&self) -> &[usize] {
        &self.slots[..self.count]
    }
}

pub(crate) struct BufferedPages<'a> {
    lbas: &'a [u32],
    cursor: usize,
}

impl<'a> BufferedPages<'a> {
    pub(crate) fn new(buffer: &'a WriteBackBuffer) -> Self {
        assert!(
            buffer.len() <= (MAXIMUM_WRITE_BUFFER_BYTES / BLOCK_SIZE as u64) as usize,
            "configured write-buffer limit"
        );
        Self {
            lbas: &buffer.lbas,
            cursor: 0,
        }
    }
}

impl Iterator for BufferedPages<'_> {
    type Item = PageGroup;

    fn next(&mut self) -> Option<Self::Item> {
        for first in self.cursor..self.lbas.len() {
            self.cursor = first + 1;
            let page_id = self.lbas[first] / ENTRIES_PER_PAGE as u32;
            if self.lbas[..first]
                .iter()
                .any(|lba| *lba / ENTRIES_PER_PAGE as u32 == page_id)
            {
                continue;
            }
            let mut group = PageGroup {
                page_id,
                slots: [0; ENTRIES_PER_PAGE],
                count: 0,
            };
            for slot in first..self.lbas.len() {
                if self.lbas[slot] / ENTRIES_PER_PAGE as u32 == page_id {
                    assert!(group.count < ENTRIES_PER_PAGE, "buffer coalesces each LBA");
                    group.slots[group.count] = slot;
                    group.count += 1;
                }
            }
            return Some(group);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v3::reservation::Reservation;
    use crate::tests::allocations::measure;
    use std::collections::BTreeMap;

    #[test]
    fn groups_interleaved_pages_once_and_preserves_coalesced_payloads() {
        let mut buffer = WriteBackBuffer::with_capacity(6);
        assert!(BufferedPages::new(&buffer).next().is_none());
        for (index, lba) in [640, 0, 320, 641, 1, 319].into_iter().enumerate() {
            assert!(buffer.store(lba, &[index as u8; BLOCK_SIZE]));
        }
        assert!(!buffer.store(641, &[99; BLOCK_SIZE]));
        let mut groups = BufferedPages::new(&buffer);
        for (page_id, expected) in [(2, &[0, 3][..]), (0, &[1, 4, 5]), (1, &[2])] {
            let group = groups.next().unwrap();
            assert_eq!(group.page_id, page_id);
            assert_eq!(group.slots(), expected);
            for slot in group.slots() {
                let byte = if *slot == 3 { 99 } else { *slot as u8 };
                assert_eq!(buffer.arena[*slot].0, [byte; BLOCK_SIZE]);
            }
        }
        assert!(groups.next().is_none());
        assert!(groups.next().is_none());
        buffer.clear();
        buffer.store(319, &[42; BLOCK_SIZE]);
        let group = BufferedPages::new(&buffer).next().unwrap();
        assert_eq!(group.page_id, 0);
        assert_eq!(group.slots(), &[0]);
    }

    #[test]
    fn groups_match_reference_and_reservation_at_buffer_limits() {
        let capacity = (MAXIMUM_WRITE_BUFFER_BYTES / BLOCK_SIZE as u64) as usize;
        let mut buffer = WriteBackBuffer::with_capacity(capacity);
        for count in [1, 320, capacity] {
            for stride in [1, 319, 320, 997] {
                buffer.clear();
                let mut reference = BTreeMap::<u32, Vec<usize>>::new();
                // Odd multiplication permutes all 4096 slots, interleaving pages.
                for slot in 0..count {
                    let lba = ((slot * 17) % capacity * stride) as u32;
                    assert!(buffer.store(lba, &[slot as u8; BLOCK_SIZE]));
                    reference.entry(lba / 320).or_default().push(slot);
                }
                let reservation = Reservation::buffered(&buffer.lbas, 0, None).unwrap();
                let mut first_slots = Vec::new();
                let mut payloads = 0;
                for group in BufferedPages::new(&buffer) {
                    assert_eq!(group.slots(), reference.remove(&group.page_id).unwrap());
                    assert!((1..=320).contains(&group.slots().len()));
                    first_slots.push(group.slots()[0]);
                    payloads += group.slots().len();
                }
                assert!(reference.is_empty());
                assert!(first_slots.windows(2).all(|pair| pair[0] < pair[1]));
                assert_eq!(payloads, count);
                assert_eq!(first_slots.len() as u64, reservation.data_records);
            }
        }
    }

    #[test]
    fn grouping_allocates_nothing_and_keeps_a_full_page_in_one_record() {
        let mut buffer = WriteBackBuffer::with_capacity(320);
        for lba in (0..320).rev() {
            buffer.store(lba, &[0; BLOCK_SIZE]);
        }
        let ((groups, payloads), allocations) = measure(|| {
            let mut groups = 0;
            let mut payloads = 0;
            for group in BufferedPages::new(&buffer) {
                groups += 1;
                payloads += group.slots().len();
                assert_eq!(group.page_id, 0);
                for (index, slot) in group.slots().iter().enumerate() {
                    assert_eq!(*slot, index);
                }
            }
            (groups, payloads)
        });
        assert_eq!((groups, payloads, allocations), (1, 320, 0));
    }
}
