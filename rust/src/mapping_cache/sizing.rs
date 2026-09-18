use super::{CacheError, ENTRIES_PER_PAGE, Slot};
use hashbrown::HashMap;
use std::hash::RandomState;

pub(super) type Lookup = HashMap<u32, u32, RandomState>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sizing {
    pub(crate) requested_bytes: u64,
    pub(crate) page_count: u32,
    pub(crate) capacity: usize,
    pub(crate) slot_bytes: usize,
    pub(crate) lookup_bytes: usize,
    pub(crate) policy_bytes: usize,
    pub(crate) allocated_bytes: usize,
}

impl Sizing {
    pub(super) fn new(logical_bytes: u64, budget: Option<u64>) -> Result<Self, CacheError> {
        let block_bytes = u64::try_from(crate::BLOCK_SIZE).expect("block size fits u64");
        let blocks = logical_bytes / block_bytes;
        if !logical_bytes.is_multiple_of(block_bytes)
            || blocks == 0
            || blocks > crate::MAX_VOLUME_BLOCKS
        {
            return Err(CacheError::InvalidGeometry);
        }
        let page_count = u32::try_from(blocks.div_ceil(ENTRIES_PER_PAGE as u64))
            .map_err(|_| CacheError::SizeOverflow)?;
        // ADR-07 starts at 0.01% of logical bytes, rounded down.
        let requested_bytes = budget.unwrap_or(logical_bytes / 10_000);
        let upper = (requested_bytes / size_of::<Slot>() as u64)
            .max(1)
            .min(u64::from(page_count));
        let mut low = 1;
        let mut high = usize::try_from(upper).map_err(|_| CacheError::SizeOverflow)?;
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if Self::for_capacity(middle)?.allocated_bytes as u64 <= requested_bytes {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        Ok(Self {
            requested_bytes,
            page_count,
            ..Self::for_capacity(low)?
        })
    }

    pub(super) fn for_capacity(capacity: usize) -> Result<Self, CacheError> {
        if capacity == 0 {
            return Err(CacheError::InvalidCapacity);
        }
        let slot_bytes = capacity
            .checked_mul(size_of::<Slot>())
            .ok_or(CacheError::SizeOverflow)?;
        let policy_bytes = capacity
            .div_ceil(u64::BITS as usize)
            .checked_mul(size_of::<u64>())
            .ok_or(CacheError::SizeOverflow)?;
        // Query the pinned implementation, including buckets, control bytes and padding.
        let lookup_bytes = new_lookup(capacity)?.allocation_size();
        let allocated_bytes = slot_bytes
            .checked_add(lookup_bytes)
            .and_then(|bytes| bytes.checked_add(policy_bytes))
            .ok_or(CacheError::SizeOverflow)?;
        Ok(Self {
            requested_bytes: 0,
            page_count: 0,
            capacity,
            slot_bytes,
            lookup_bytes,
            policy_bytes,
            allocated_bytes,
        })
    }
}

pub(super) fn new_lookup(capacity: usize) -> Result<Lookup, CacheError> {
    let reserve = capacity.checked_mul(2).ok_or(CacheError::SizeOverflow)?;
    let mut lookup = Lookup::with_hasher(RandomState::new());
    // hashbrown 0.17.1 raw::reserve_rehash_inner rehashes in place when the next
    // length is <= half its full capacity. Reserve twice the maximum live slots
    // so deletion tombstones cannot cause allocation during replacement.
    lookup
        .try_reserve(reserve)
        .map_err(|_| CacheError::AllocationFailed)?;
    Ok(lookup)
}

pub(super) fn filled_slice<T: Clone>(length: usize, value: T) -> Result<Box<[T]>, CacheError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(length)
        .map_err(|_| CacheError::AllocationFailed)?;
    entries.resize(length, value);
    Ok(entries.into_boxed_slice())
}
