use super::{CacheError, sizing::filled_slice};

pub(super) struct Clock {
    referenced: Box<[u64]>,
    capacity: usize,
    cursor: usize,
}

impl Clock {
    pub(super) fn new(capacity: usize) -> Result<Self, CacheError> {
        if capacity == 0 {
            return Err(CacheError::InvalidCapacity);
        }
        Ok(Self {
            referenced: filled_slice(capacity.div_ceil(u64::BITS as usize), 0)?,
            capacity,
            cursor: 0,
        })
    }

    pub(super) fn access(&mut self, slot: usize) {
        self.set(slot, true);
    }

    pub(super) fn insert(&mut self, slot: usize) {
        self.set(slot, true);
    }

    pub(super) fn remove(&mut self, slot: usize) {
        self.set(slot, false);
    }

    fn set(&mut self, slot: usize, value: bool) {
        assert!(slot < self.capacity, "CLOCK slot must be in range");
        let word = self.referenced.get_mut(slot / 64).expect("allocated bit");
        let mask = 1_u64 << (slot % 64);
        if value {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    pub(super) fn victim(&mut self, clean: impl Fn(usize) -> bool) -> (Option<usize>, u64) {
        let mut examined = 0;
        for _ in 0..2 {
            for _ in 0..self.capacity {
                let slot = self.cursor;
                self.cursor = (self.cursor + 1) % self.capacity;
                examined += 1;
                let referenced = self.referenced.get(slot / 64).expect("allocated bit")
                    & (1_u64 << (slot % 64))
                    != 0;
                if referenced {
                    self.remove(slot);
                } else if clean(slot) {
                    return (Some(slot), examined);
                }
            }
        }
        (None, examined)
    }

    pub(super) fn allocated_bytes(&self) -> usize {
        std::mem::size_of_val(self.referenced.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_capacity() {
        assert!(matches!(Clock::new(0), Err(CacheError::InvalidCapacity)));
    }

    #[test]
    fn second_chances_access_and_cursor_wrap() {
        let mut clock = Clock::new(3).unwrap();
        for slot in 0..3 {
            clock.insert(slot);
        }
        assert_eq!(clock.victim(|_| true), (Some(0), 4));
        clock.insert(0);
        clock.access(1);
        assert_eq!(clock.victim(|_| true), (Some(2), 2));
        clock.insert(2);
        assert_eq!(clock.victim(|_| true), (Some(1), 2));
        clock.remove(1);
        assert_eq!(clock.victim(|_| true), (Some(0), 2));
    }

    #[test]
    fn dirty_slots_are_skipped_and_scan_is_bounded() {
        let mut clock = Clock::new(3).unwrap();
        for slot in 0..3 {
            clock.insert(slot);
        }
        assert_eq!(clock.victim(|_| false), (None, 6));
        assert_eq!(clock.victim(|_| false), (None, 6));
        assert_eq!(clock.victim(|slot| slot == 2), (Some(2), 3));
    }

    #[test]
    fn one_slot_and_bitset_word_boundaries() {
        for capacity in [1, 63, 64, 65, 129] {
            let mut clock = Clock::new(capacity).unwrap();
            for slot in 0..capacity {
                clock.insert(slot);
            }
            assert_eq!(clock.allocated_bytes(), capacity.div_ceil(64) * 8);
            assert_eq!(
                clock.victim(|slot| slot == capacity - 1),
                (Some(capacity - 1), (2 * capacity) as u64)
            );
        }
    }

    #[test]
    #[should_panic(expected = "CLOCK slot must be in range")]
    fn rejects_inconsistent_slot_event() {
        Clock::new(1).unwrap().access(1);
    }
}
