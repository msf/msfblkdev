//! The total heap budget and its division. Everything the engine allocates after open belongs
//! to one of four shares; the sum must fit the proportional total.
use super::Geometry;
use crate::{BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS, invalid_input, mapping_cache::sizing::Sizing};
use std::io;

/// Default proportion of the logical volume: 1% (parts per million).
pub const DEFAULT_HEAP_PROPORTION_PPM: u32 = 10_000;
/// Floor for tiny volumes: the smallest total that still holds one packed record, one cleaner
/// window and one cache slot with headroom. Documented, not silent.
pub const MINIMUM_HEAP_BYTES: u64 = 16 * 1024 * 1024;

/// How the caller wants the total divided. Explicit requests are validated, never enlarged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapBudgetOptions {
    /// Parts per million of the logical volume size that the total heap may use.
    pub proportion_ppm: u32,
    /// Replaces the proportional total, mainly for tests of deliberately small budgets.
    pub total_bytes: Option<u64>,
    /// Bytes for cleaner read windows and proposal storage; None uses the write buffer size.
    pub cleaner_bytes: Option<u64>,
}

impl Default for HeapBudgetOptions {
    fn default() -> Self {
        Self {
            proportion_ppm: DEFAULT_HEAP_PROPORTION_PPM,
            total_bytes: None,
            cleaner_bytes: None,
        }
    }
}

/// The validated division of the heap budget, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapBudget {
    pub total_bytes: u64,
    pub write_buffer_bytes: u64,
    pub cleaner_bytes: u64,
    pub cache_bytes: u64,
    /// Resident index, segment state, scratch, queues and other bookkeeping.
    pub supporting_bytes: u64,
}

impl HeapBudget {
    pub fn allocated_bytes(&self) -> u64 {
        self.write_buffer_bytes + self.cleaner_bytes + self.cache_bytes + self.supporting_bytes
    }

    pub fn summary(&self) -> String {
        format!(
            "heap_total_bytes={} heap_write_buffer_bytes={} heap_cleaner_bytes={} heap_cache_bytes={} heap_supporting_bytes={} heap_allocated_bytes={}",
            self.total_bytes,
            self.write_buffer_bytes,
            self.cleaner_bytes,
            self.cache_bytes,
            self.supporting_bytes,
            self.allocated_bytes()
        )
    }
}

/// Fixed-size cleaner storage derived from the segment size and the cleaner share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CleanerShape {
    /// Blocks per read window: one maximal record, or the whole segment when smaller.
    pub(crate) window_blocks: u32,
    pub(crate) proposal_buffers: u32,
}

impl CleanerShape {
    pub(crate) fn window_blocks(geometry: Geometry) -> u32 {
        (MAXIMUM_PAYLOAD_BLOCKS as u32 + 1).min(geometry.blocks_per_segment)
    }

    /// Bytes of one window buffer: its block arena plus one entry and one order slot per block.
    pub(crate) fn proposal_buffer_bytes(geometry: Geometry) -> u64 {
        let blocks = u64::from(Self::window_blocks(geometry));
        blocks * (BLOCK_SIZE as u64 + super::reclaim::ENTRY_BYTES as u64 + 4)
    }

    /// Scanner scratch: one snapshot block, the decoded page cache and the accepted-index list.
    pub(crate) fn scanner_bytes(geometry: Geometry) -> u64 {
        BLOCK_SIZE as u64
            + super::reclaim::scanner::PAGE_CACHE_BYTES as u64
            + u64::from(Self::window_blocks(geometry)) * 4
    }

    /// The smallest useful cleaner share: the scanner scratch and one window buffer.
    pub(crate) fn minimum_bytes(geometry: Geometry) -> u64 {
        Self::scanner_bytes(geometry) + Self::proposal_buffer_bytes(geometry)
    }

    pub(crate) fn fit(geometry: Geometry, cleaner_bytes: u64) -> io::Result<Self> {
        let remaining = cleaner_bytes
            .checked_sub(Self::scanner_bytes(geometry))
            .ok_or_else(|| invalid_input("cleaner budget cannot hold the scanner scratch"))?;
        let buffers = remaining / Self::proposal_buffer_bytes(geometry);
        if buffers == 0 {
            return Err(invalid_input(
                "cleaner budget cannot hold one window buffer",
            ));
        }
        Ok(Self {
            window_blocks: Self::window_blocks(geometry),
            proposal_buffers: u32::try_from(buffers.min(64)).expect("bounded"),
        })
    }

    pub(crate) fn allocated_bytes(self, geometry: Geometry) -> u64 {
        Self::scanner_bytes(geometry)
            + u64::from(self.proposal_buffers) * Self::proposal_buffer_bytes(geometry)
    }
}

/// Inputs that determine the supporting share.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SupportingState {
    pub(crate) resident_index_bytes: u64,
    pub(crate) segment_state_bytes: u64,
    pub(crate) write_buffer_lookup_bytes: u64,
    pub(crate) fixed_scratch_bytes: u64,
}

impl SupportingState {
    pub(crate) fn total(self) -> u64 {
        self.resident_index_bytes
            + self.segment_state_bytes
            + self.write_buffer_lookup_bytes
            + self.fixed_scratch_bytes
    }
}

/// Divides the total. `cache_request` is the caller's explicit cache budget, if any.
pub(crate) fn divide(
    options: HeapBudgetOptions,
    logical_bytes: u64,
    write_buffer_bytes: u64,
    cache_request: Option<u64>,
    cache_minimum_slots: usize,
    geometry: Geometry,
    supporting: SupportingState,
) -> io::Result<(HeapBudget, Sizing, CleanerShape)> {
    let proportional = logical_bytes / 1_000_000 * u64::from(options.proportion_ppm)
        + (logical_bytes % 1_000_000) * u64::from(options.proportion_ppm) / 1_000_000;
    let total_bytes = options
        .total_bytes
        .unwrap_or(proportional.max(MINIMUM_HEAP_BYTES));
    // The cleaner share follows the write buffer but never drops below one window and one
    // proposal buffer, so write-through configurations still open.
    let cleaner_bytes = options
        .cleaner_bytes
        .unwrap_or(write_buffer_bytes.max(CleanerShape::minimum_bytes(geometry)));
    let cleaner = CleanerShape::fit(geometry, cleaner_bytes)?;
    let supporting_bytes = supporting.total();
    let committed = write_buffer_bytes + cleaner_bytes + supporting_bytes;
    let remaining = total_bytes.checked_sub(committed).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "heap budget of {total_bytes} bytes cannot hold write buffer {write_buffer_bytes}, cleaner {cleaner_bytes} and supporting state {supporting_bytes}"
            ),
        )
    })?;
    let minimum = Sizing::for_capacity(cache_minimum_slots).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mapping cache: {error:?}"),
        )
    })?;
    let cache_request = cache_request.unwrap_or(remaining);
    if cache_request > remaining {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mapping cache request of {cache_request} bytes exceeds the {remaining} bytes left in the heap budget"
            ),
        ));
    }
    let sizing = Sizing::new(logical_bytes, Some(cache_request)).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mapping cache: {error:?}"),
        )
    })?;
    if sizing.capacity < cache_minimum_slots {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mapping cache has {} slots; the write buffer requires {cache_minimum_slots} slots (at least {} cache bytes)",
                sizing.capacity, minimum.allocated_bytes
            ),
        ));
    }
    Ok((
        HeapBudget {
            total_bytes,
            write_buffer_bytes,
            cleaner_bytes: cleaner.allocated_bytes(geometry),
            cache_bytes: sizing.allocated_bytes as u64,
            supporting_bytes,
        },
        sizing,
        cleaner,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supporting() -> SupportingState {
        SupportingState {
            resident_index_bytes: 4_000,
            segment_state_bytes: 3_000,
            write_buffer_lookup_bytes: 2_000,
            fixed_scratch_bytes: 150_000,
        }
    }

    #[test]
    fn division_is_proportional_floored_and_rejects_oversubscription() {
        let geometry = Geometry::new(262_144, 2 + 128 * 2048, 2048).unwrap();
        let logical = 1_u64 << 30;
        let (budget, sizing, cleaner) = divide(
            HeapBudgetOptions::default(),
            logical,
            4 << 20,
            None,
            16,
            geometry,
            supporting(),
        )
        .unwrap();
        assert_eq!(budget.total_bytes, 16 << 20);
        assert_eq!(budget.write_buffer_bytes, 4 << 20);
        assert_eq!(cleaner.window_blocks, 339);
        assert_eq!(cleaner.proposal_buffers, 2);
        assert_eq!(budget.cleaner_bytes, cleaner.allocated_bytes(geometry));
        assert!(
            budget.cache_bytes <= budget.total_bytes - (4 << 20) - budget.cleaner_bytes - 159_000
        );
        assert!(budget.allocated_bytes() <= budget.total_bytes);
        assert!(sizing.capacity >= 16);
        // Ten GiB: the proportion exceeds the floor.
        let (budget, _, _) = divide(
            HeapBudgetOptions::default(),
            10 << 30,
            4 << 20,
            Some(1 << 20),
            16,
            geometry,
            supporting(),
        )
        .unwrap();
        assert_eq!(budget.total_bytes, (10 << 30) / 100);
        assert!(budget.cache_bytes <= 1 << 20);
        // A twelve MiB explicit total with a four MiB buffer and a two MiB cleaner share.
        let small = HeapBudgetOptions {
            total_bytes: Some(12 << 20),
            cleaner_bytes: Some(CleanerShape::minimum_bytes(geometry)),
            ..HeapBudgetOptions::default()
        };
        let (budget, _, cleaner) =
            divide(small, logical, 4 << 20, None, 16, geometry, supporting()).unwrap();
        assert_eq!(budget.total_bytes, 12 << 20);
        assert_eq!(cleaner.proposal_buffers, 1);
        assert!(budget.allocated_bytes() <= 12 << 20);
        // Oversubscription and undersized cleaner shares are rejected before mutation.
        let tiny = HeapBudgetOptions {
            total_bytes: Some(5 << 20),
            ..HeapBudgetOptions::default()
        };
        assert!(divide(tiny, logical, 4 << 20, None, 16, geometry, supporting()).is_err());
        let starved = HeapBudgetOptions {
            cleaner_bytes: Some(1 << 20),
            ..HeapBudgetOptions::default()
        };
        assert!(divide(starved, logical, 4 << 20, None, 16, geometry, supporting()).is_err());
        assert!(
            divide(
                HeapBudgetOptions::default(),
                logical,
                4 << 20,
                Some(100 << 20),
                16,
                geometry,
                supporting()
            )
            .is_err()
        );
        assert!(
            divide(
                HeapBudgetOptions::default(),
                logical,
                4 << 20,
                Some(4_000),
                16,
                geometry,
                supporting()
            )
            .is_err()
        );
        // Small segments shrink the cleaner's fixed shapes.
        let tiny_segments = Geometry::new(1000, 2 + 12 * 8, 8).unwrap();
        assert_eq!(CleanerShape::window_blocks(tiny_segments), 8);
        assert_eq!(
            CleanerShape::minimum_bytes(tiny_segments),
            CleanerShape::scanner_bytes(tiny_segments) + 8 * (4096 + 24 + 4)
        );
    }
}
