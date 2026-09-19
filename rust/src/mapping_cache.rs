//! ADR-07's bounded cache. No backing I/O or persistence state lives here.
#![forbid(unsafe_code)]

mod clock;
pub(crate) mod sizing;
#[cfg(test)]
mod tests;

use clock::Clock;
use sizing::{Lookup, Sizing, filled_slice, new_lookup};

// ADR-07 packs 336 physical addresses and payload checksums into each mapping page.
pub(super) const ENTRIES_PER_PAGE: usize = 336;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MappingEntries {
    pub(super) physical_addr: [u32; ENTRIES_PER_PAGE],
    pub(super) payload_checksum: [u64; ENTRIES_PER_PAGE],
}

impl MappingEntries {
    pub(super) fn zeroed() -> Self {
        Self {
            physical_addr: [0; ENTRIES_PER_PAGE],
            payload_checksum: [0; ENTRIES_PER_PAGE],
        }
    }

    fn validate(&self, valid_entries: usize) -> Result<(), CacheError> {
        for (index, (&physical, &checksum)) in self
            .physical_addr
            .iter()
            .zip(&self.payload_checksum)
            .enumerate()
        {
            if (physical == 0 && checksum != 0)
                || (index >= valid_entries && (physical != 0 || checksum != 0))
            {
                return Err(CacheError::InvalidMapping);
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Slot {
    entries: MappingEntries,
    page_id: u32,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CacheError {
    InvalidGeometry,
    InvalidCapacity,
    InvalidPage,
    InvalidEntry,
    InvalidMapping,
    SizeOverflow,
    AllocationFailed,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Access {
    Foreground,
    Recovery,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lookups {
    pub hits: u64,
    pub misses: u64,
}

impl Lookups {
    pub fn hit_rate(self) -> Option<f64> {
        let total = u128::from(self.hits) + u128::from(self.misses);
        (total != 0).then(|| self.hits as f64 / total as f64)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub foreground: Lookups,
    pub recovery: Lookups,
    pub clean_evictions: u64,
    pub victim_slots_examined: u64,
    pub checkpoint_required: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub counters: Counters,
    pub resident_slots: usize,
    pub dirty_slots: usize,
    pub sizing: Sizing,
}

pub(super) struct MappingCache {
    slots: Box<[Slot]>,
    lookup: Lookup,
    clock: Clock,
    sizing: Sizing,
    volume_blocks: u32,
    resident: usize,
    counters: Counters,
}

// A retry carries the already-counted acquisition, not a second hit or miss.
pub(super) struct PendingPage(u32);

pub(super) enum Acquisition<'a> {
    Resident(CachedPage<'a>),
    Vacant(VacantPage<'a>),
    CheckpointRequired(PendingPage),
}

pub(super) struct CachedPage<'a> {
    slot: &'a mut Slot,
    valid_entries: usize,
}

pub(super) struct VacantPage<'a> {
    cache: &'a mut MappingCache,
    page_id: u32,
    slot: usize,
}

impl MappingCache {
    pub(super) fn new(logical_bytes: u64, budget: Option<u64>) -> Result<Self, CacheError> {
        let sizing = Sizing::new(logical_bytes, budget)?;
        let lookup = new_lookup(sizing.capacity)?;
        let clock = Clock::new(sizing.capacity)?;
        let slots = filled_slice(
            sizing.capacity,
            Slot {
                entries: MappingEntries::zeroed(),
                page_id: 0,
                dirty: false,
            },
        )?;
        assert_eq!(
            lookup.allocation_size(),
            sizing.lookup_bytes,
            "lookup allocation must match sizing"
        );
        assert_eq!(
            clock.allocated_bytes(),
            sizing.policy_bytes,
            "policy allocation must match sizing"
        );
        assert_eq!(
            std::mem::size_of_val(slots.as_ref()),
            sizing.slot_bytes,
            "slot allocation must match sizing"
        );
        Ok(Self {
            slots,
            lookup,
            clock,
            sizing,
            volume_blocks: u32::try_from(logical_bytes / crate::BLOCK_SIZE as u64)
                .expect("validated geometry"),
            resident: 0,
            counters: Counters::default(),
        })
    }

    pub(super) fn acquire(
        &mut self,
        page_id: u32,
        access: Access,
    ) -> Result<Acquisition<'_>, CacheError> {
        self.validate_page(page_id)?;
        let hit = self.lookup.contains_key(&page_id);
        let counters = match access {
            Access::Foreground => &mut self.counters.foreground,
            Access::Recovery => &mut self.counters.recovery,
        };
        if hit {
            counters.hits += 1;
        } else {
            counters.misses += 1;
        }
        Ok(self.resolve(page_id))
    }

    pub(super) fn retry(&mut self, pending: PendingPage) -> Result<Acquisition<'_>, CacheError> {
        self.validate_page(pending.0)?;
        Ok(self.resolve(pending.0))
    }

    fn validate_page(&self, page_id: u32) -> Result<(), CacheError> {
        if page_id >= self.sizing.page_count {
            return Err(CacheError::InvalidPage);
        }
        Ok(())
    }

    fn valid_entries(&self, page_id: u32) -> usize {
        assert!(page_id < self.sizing.page_count, "validated page ID");
        usize::try_from(self.volume_blocks - page_id * ENTRIES_PER_PAGE as u32)
            .expect("volume blocks fit usize")
            .min(ENTRIES_PER_PAGE)
    }

    fn resolve(&mut self, page_id: u32) -> Acquisition<'_> {
        if let Some(&slot) = self.lookup.get(&page_id) {
            let slot = usize::try_from(slot).expect("slot fits usize");
            self.clock.access(slot);
            let valid_entries = self.valid_entries(page_id);
            return Acquisition::Resident(CachedPage {
                slot: self.slots.get_mut(slot).expect("resident slot in range"),
                valid_entries,
            });
        }
        let slot = if self.resident < self.slots.len() {
            Some(self.resident)
        } else {
            let (victim, examined) = self
                .clock
                .victim(|slot| !self.slots.get(slot).expect("CLOCK slot in range").dirty);
            self.counters.victim_slots_examined += examined;
            victim
        };
        match slot {
            Some(slot) => Acquisition::Vacant(VacantPage {
                cache: self,
                page_id,
                slot,
            }),
            None => {
                self.counters.checkpoint_required += 1;
                Acquisition::CheckpointRequired(PendingPage(page_id))
            }
        }
    }

    pub(super) fn dirty_pages(&self) -> impl Iterator<Item = (u32, &MappingEntries)> {
        self.slots
            .iter()
            .take(self.resident)
            .filter(|slot| slot.dirty)
            .map(|slot| (slot.page_id, &slot.entries))
    }

    /// The caller must invoke this only after successful checkpoint publication.
    pub(super) fn checkpoint_published(&mut self) {
        for slot in self.slots.iter_mut().take(self.resident) {
            slot.dirty = false;
        }
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        Snapshot {
            counters: self.counters,
            resident_slots: self.resident,
            dirty_slots: self.dirty_pages().count(),
            sizing: self.sizing,
        }
    }
}

impl CachedPage<'_> {
    pub(super) fn entries(&self) -> &MappingEntries {
        &self.slot.entries
    }

    pub(super) fn update(
        &mut self,
        entry: usize,
        physical: u32,
        checksum: u64,
    ) -> Result<(), CacheError> {
        if entry >= self.valid_entries {
            return Err(CacheError::InvalidEntry);
        }
        if physical == 0 && checksum != 0 {
            return Err(CacheError::InvalidMapping);
        }
        *self
            .slot
            .entries
            .physical_addr
            .get_mut(entry)
            .expect("validated entry") = physical;
        *self
            .slot
            .entries
            .payload_checksum
            .get_mut(entry)
            .expect("validated entry") = checksum;
        self.slot.dirty = true;
        Ok(())
    }
}

impl<'a> VacantPage<'a> {
    pub(super) fn install(self, entries: MappingEntries) -> Result<CachedPage<'a>, CacheError> {
        let valid_entries = self.cache.valid_entries(self.page_id);
        entries.validate(valid_entries)?;
        let slot = self
            .cache
            .slots
            .get_mut(self.slot)
            .expect("reserved slot in range");
        assert!(!slot.dirty, "dirty pages cannot be replaced");
        if self.slot < self.cache.resident {
            let removed = self.cache.lookup.remove(&slot.page_id);
            assert_eq!(
                removed,
                Some(u32::try_from(self.slot).expect("slot fits")),
                "victim must be indexed"
            );
            self.cache.clock.remove(self.slot);
            self.cache.counters.clean_evictions += 1;
        } else {
            assert_eq!(
                self.slot, self.cache.resident,
                "unused slots fill sequentially"
            );
            self.cache.resident += 1;
        }
        let previous = self
            .cache
            .lookup
            .insert(self.page_id, u32::try_from(self.slot).expect("slot fits"));
        assert!(previous.is_none(), "page cannot occupy two slots");
        assert_eq!(
            self.cache.lookup.allocation_size(),
            self.cache.sizing.lookup_bytes,
            "lookup must not grow"
        );
        *slot = Slot {
            entries,
            page_id: self.page_id,
            dirty: false,
        };
        self.cache.clock.insert(self.slot);
        Ok(CachedPage {
            slot,
            valid_entries,
        })
    }
}
