use crate::mapping_cache::ENTRIES_PER_PAGE;
use crate::{BLOCK_SIZE, MAX_BACKING_BLOCKS, MAX_VOLUME_BLOCKS, invalid_data};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub(crate) volume_blocks: u32,
    pub(crate) backing_blocks: u64,
    pub(crate) page_count: u32,
    pub(crate) index_blocks: u32,
    pub(crate) log_start: u32,
}

impl Geometry {
    pub(crate) fn new(volume_blocks: u32, backing_blocks: u64) -> io::Result<Self> {
        if volume_blocks == 0
            || u64::from(volume_blocks) > MAX_VOLUME_BLOCKS
            || backing_blocks > MAX_BACKING_BLOCKS
        {
            return Err(invalid_data("invalid version-3 geometry"));
        }
        let page_count = volume_blocks.div_ceil(ENTRIES_PER_PAGE as u32);
        let index_blocks = page_count.div_ceil((BLOCK_SIZE / size_of::<u32>()) as u32);
        let log_start = 2 + 2 * index_blocks;
        // One data payload, its footer, and the mapping snapshot needed to checkpoint it.
        if backing_blocks < u64::from(log_start) + 3 {
            return Err(invalid_data(
                "backing cannot hold a checkpointable data record",
            ));
        }
        Ok(Self {
            volume_blocks,
            backing_blocks,
            page_count,
            index_blocks,
            log_start,
        })
    }

    pub(crate) fn page_entries(self, page_id: u32) -> io::Result<usize> {
        if page_id >= self.page_count {
            return Err(invalid_data("mapping page ID out of range"));
        }
        Ok(
            (self.volume_blocks - page_id * ENTRIES_PER_PAGE as u32).min(ENTRIES_PER_PAGE as u32)
                as usize,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_bounded_and_rounds_index_bodies_once() {
        let minimum = Geometry::new(1, 7).unwrap();
        assert_eq!(
            (minimum.page_count, minimum.index_blocks, minimum.log_start),
            (1, 1, 4)
        );
        assert_eq!(minimum.page_entries(0).unwrap(), 1);
        assert!(minimum.page_entries(1).is_err());
        for volume in [320, 321, 320 * 1024, 320 * 1024 + 1] {
            let geometry = Geometry::new(volume, 100_000).unwrap();
            assert_eq!(geometry.page_count, volume.div_ceil(320));
            assert_eq!(geometry.index_blocks, volume.div_ceil(320 * 1024));
            assert_eq!(
                geometry.page_entries(geometry.page_count - 1).unwrap(),
                ((volume - 1) % 320 + 1) as usize
            );
        }
        let maximum = Geometry::new(1 << 31, 1_u64 << 32).unwrap();
        assert_eq!(maximum.page_count, 6_710_887);
        assert_eq!(maximum.index_blocks, 6_554);
        assert_eq!(maximum.log_start, 13_110);
        for (volume, backing) in [
            (0, 100),
            ((1 << 31) + 1, 100),
            (1, (1_u64 << 32) + 1),
            (1, 6),
        ] {
            assert!(Geometry::new(volume, backing).is_err());
        }
    }
}
