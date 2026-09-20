use super::FIRST_SEGMENT_BLOCK;
use crate::mapping_cache::ENTRIES_PER_PAGE;
use crate::{BLOCK_SIZE, MAX_BACKING_BLOCKS, MAX_VOLUME_BLOCKS, invalid_data};
use std::io;

/// Child pointers per `MappingIndexPage`: (4072 - 40) / 4.
pub(crate) const INDEX_CHILDREN: usize = 1008;
/// Direct index pointers held by the descriptor.
pub(crate) const DIRECT_POINTERS: usize = 64;
/// Indirect index pointers held by the descriptor.
pub(crate) const INDIRECT_POINTERS: usize = 8;
/// Segments per `SegmentTablePage`: (4072 - 40) / (8 + 1).
pub(crate) const TABLE_ENTRIES: usize = 448;
/// Segment table pointers held by the descriptor.
pub(crate) const TABLE_POINTERS: usize = 930;
/// The largest mapping page count the tree can address.
pub(crate) const MAX_MAPPING_PAGES: u64 =
    (DIRECT_POINTERS + INDIRECT_POINTERS * INDEX_CHILDREN) as u64 * INDEX_CHILDREN as u64;
/// One link plus a one-payload data record plus one one-block metadata record.
pub(crate) const MIN_BLOCKS_PER_SEGMENT: u32 = 4;
/// Two open heads and one segment to allocate.
pub(crate) const MIN_SEGMENTS: u32 = 3;
pub(crate) const MAX_SEGMENTS: u64 = (TABLE_POINTERS * TABLE_ENTRIES) as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub(crate) volume_blocks: u32,
    /// The provisioned backing bound; segments beyond it do not exist yet.
    pub(crate) backing_blocks: u64,
    pub(crate) blocks_per_segment: u32,
    pub(crate) segment_count: u32,
    pub(crate) page_count: u32,
    pub(crate) direct_pages: u32,
    pub(crate) indirect_pages: u32,
    pub(crate) table_pages: u32,
}

impl Geometry {
    pub(crate) fn new(
        volume_blocks: u32,
        backing_blocks: u64,
        blocks_per_segment: u32,
    ) -> io::Result<Self> {
        if volume_blocks == 0
            || u64::from(volume_blocks) > MAX_VOLUME_BLOCKS
            || backing_blocks > MAX_BACKING_BLOCKS
            || blocks_per_segment < MIN_BLOCKS_PER_SEGMENT
        {
            return Err(invalid_data("invalid format-4 geometry"));
        }
        let segment_count = backing_blocks.saturating_sub(u64::from(FIRST_SEGMENT_BLOCK))
            / u64::from(blocks_per_segment);
        if segment_count < u64::from(MIN_SEGMENTS) {
            return Err(invalid_data("backing holds fewer than three segments"));
        }
        if segment_count > MAX_SEGMENTS {
            return Err(invalid_data("segment table cannot address every segment"));
        }
        let page_count = u64::from(volume_blocks).div_ceil(ENTRIES_PER_PAGE as u64);
        assert!(page_count <= MAX_MAPPING_PAGES, "index tree covers 8 TiB");
        let direct_pages = page_count.div_ceil(INDEX_CHILDREN as u64);
        let indirect_pages = direct_pages
            .saturating_sub(DIRECT_POINTERS as u64)
            .div_ceil(INDEX_CHILDREN as u64);
        let table_pages = segment_count.div_ceil(TABLE_ENTRIES as u64);
        Ok(Self {
            volume_blocks,
            backing_blocks,
            blocks_per_segment,
            segment_count: u32::try_from(segment_count).expect("bounded segment count"),
            page_count: u32::try_from(page_count).expect("bounded page count"),
            direct_pages: u32::try_from(direct_pages).expect("bounded direct pages"),
            indirect_pages: u32::try_from(indirect_pages).expect("bounded indirect pages"),
            table_pages: u32::try_from(table_pages).expect("bounded table pages"),
        })
    }

    /// The geometry after backing growth. The volume and segment size never change.
    pub(crate) fn with_backing(self, backing_blocks: u64) -> io::Result<Self> {
        let grown = Self::new(self.volume_blocks, backing_blocks, self.blocks_per_segment)?;
        if grown.segment_count < self.segment_count {
            return Err(invalid_data("backing cannot shrink"));
        }
        Ok(grown)
    }

    /// Eight MiB segments up to 2 GiB of backing, then the smallest of 16 and 32 MiB whose
    /// table reach covers the backing. Larger backings need larger segments or another level.
    pub(crate) fn default_blocks_per_segment(backing_bytes: u64) -> u32 {
        const MIB: u64 = 1024 * 1024;
        for segment_bytes in [8 * MIB, 16 * MIB, 32 * MIB] {
            let reach = segment_bytes * MAX_SEGMENTS;
            let small = segment_bytes == 8 * MIB && backing_bytes <= 2048 * MIB;
            if small || (segment_bytes > 8 * MIB && backing_bytes <= reach) {
                return u32::try_from(segment_bytes / BLOCK_SIZE as u64).expect("small");
            }
        }
        u32::try_from(32 * MIB / BLOCK_SIZE as u64).expect("small")
    }

    pub(crate) fn segment_area_end(self) -> u64 {
        u64::from(FIRST_SEGMENT_BLOCK)
            + u64::from(self.segment_count) * u64::from(self.blocks_per_segment)
    }

    pub(crate) fn segment_start(self, segment: u32) -> io::Result<u32> {
        if segment >= self.segment_count {
            return Err(invalid_data("segment ID out of range"));
        }
        let start = u64::from(FIRST_SEGMENT_BLOCK)
            + u64::from(segment) * u64::from(self.blocks_per_segment);
        u32::try_from(start).map_err(|_| invalid_data("segment start exceeds u32"))
    }

    /// One past the last block of `segment`.
    pub(crate) fn segment_end(self, segment: u32) -> io::Result<u64> {
        Ok(u64::from(self.segment_start(segment)?) + u64::from(self.blocks_per_segment))
    }

    pub(crate) fn segment_of(self, block: u32) -> io::Result<u32> {
        if block < FIRST_SEGMENT_BLOCK || u64::from(block) >= self.segment_area_end() {
            return Err(invalid_data("physical block outside every whole segment"));
        }
        Ok((block - FIRST_SEGMENT_BLOCK) / self.blocks_per_segment)
    }

    /// Whether a nonzero pointer or address can name a record block.
    pub(crate) fn contains_block(self, block: u32) -> bool {
        block >= FIRST_SEGMENT_BLOCK && u64::from(block) < self.segment_area_end()
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

    /// The direct index page and child slot that locate `mapping_page`.
    pub(crate) fn direct_slot(self, mapping_page: u32) -> (u32, usize) {
        (
            mapping_page / INDEX_CHILDREN as u32,
            (mapping_page % INDEX_CHILDREN as u32) as usize,
        )
    }

    /// Where direct page `direct` is pointed at: the descriptor array or an indirect page slot.
    pub(crate) fn direct_parent(self, direct: u32) -> DirectParent {
        if direct < DIRECT_POINTERS as u32 {
            DirectParent::Descriptor(direct as usize)
        } else {
            let offset = direct - DIRECT_POINTERS as u32;
            DirectParent::Indirect {
                indirect: offset / INDEX_CHILDREN as u32,
                slot: (offset % INDEX_CHILDREN as u32) as usize,
            }
        }
    }

    pub(crate) fn table_slot(self, segment: u32) -> (u32, usize) {
        (
            segment / TABLE_ENTRIES as u32,
            (segment % TABLE_ENTRIES as u32) as usize,
        )
    }

    /// Mapping pages covered by direct page `direct`.
    pub(crate) fn direct_children(self, direct: u32) -> usize {
        let first = u64::from(direct) * INDEX_CHILDREN as u64;
        usize::try_from(
            u64::from(self.page_count)
                .saturating_sub(first)
                .min(INDEX_CHILDREN as u64),
        )
        .expect("bounded")
    }

    /// Direct pages covered by indirect page `indirect`.
    pub(crate) fn indirect_children(self, indirect: u32) -> usize {
        let first = DIRECT_POINTERS as u64 + u64::from(indirect) * INDEX_CHILDREN as u64;
        usize::try_from(
            u64::from(self.direct_pages)
                .saturating_sub(first)
                .min(INDEX_CHILDREN as u64),
        )
        .expect("bounded")
    }

    /// Segments covered by table page `table`.
    pub(crate) fn table_children(self, table: u32) -> usize {
        let first = u64::from(table) * TABLE_ENTRIES as u64;
        usize::try_from(
            u64::from(self.segment_count)
                .saturating_sub(first)
                .min(TABLE_ENTRIES as u64),
        )
        .expect("bounded")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectParent {
    Descriptor(usize),
    Indirect { indirect: u32, slot: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_derives_segments_pages_and_tree_shape() {
        let small = Geometry::new(1, 2 + 3 * 4, 4).unwrap();
        assert_eq!(
            (
                small.segment_count,
                small.page_count,
                small.direct_pages,
                small.indirect_pages,
                small.table_pages
            ),
            (3, 1, 1, 0, 1)
        );
        assert_eq!(small.segment_start(0).unwrap(), 2);
        assert_eq!(small.segment_start(2).unwrap(), 10);
        assert_eq!(small.segment_end(2).unwrap(), 14);
        assert_eq!(small.segment_area_end(), 14);
        assert!(small.segment_start(3).is_err());
        for (block, segment) in [(2, 0), (5, 0), (6, 1), (13, 2)] {
            assert_eq!(small.segment_of(block).unwrap(), segment);
            assert!(small.contains_block(block));
        }
        for block in [0, 1, 14, 15, u32::MAX] {
            assert!(small.segment_of(block).is_err());
            assert!(!small.contains_block(block));
        }
        // A partial trailing segment is not addressable.
        let partial = Geometry::new(1, 2 + 3 * 4 + 3, 4).unwrap();
        assert_eq!(partial.segment_count, 3);
        assert_eq!(partial.segment_area_end(), 14);

        // 128 MiB logical: 98 pages, one direct page; 192 MiB backing with 16 MiB segments.
        let example = Geometry::new(32_768, 2 + 12 * 4096, 4096).unwrap();
        assert_eq!((example.page_count, example.direct_pages), (98, 1));
        assert_eq!(example.page_entries(97).unwrap(), 32_768 - 97 * 336);
        assert!(example.page_entries(98).is_err());
        assert_eq!(example.direct_slot(97), (0, 97));
        assert_eq!(example.direct_parent(0), DirectParent::Descriptor(0));
        assert_eq!(example.direct_children(0), 98);
        assert_eq!(example.table_slot(11), (0, 11));
        assert_eq!(example.table_children(0), 12);

        // 4 TiB backing, 16 MiB segments: 262,143 segments and 586 table pages (ADR-08 text).
        let large = Geometry::new(1 << 31, 1 << 30, 4096).unwrap();
        assert_eq!(large.segment_count, 262_143);
        assert_eq!(large.table_pages, 586);
        assert_eq!(large.page_count, 6_391_321);
        assert_eq!(large.direct_pages, 6_341);
        assert_eq!(large.indirect_pages, 7);
        assert_eq!(large.direct_children(6_340), 6_391_321 - 6_340 * 1008);
        assert_eq!(large.indirect_children(6), 6_341 - 64 - 6 * 1008);
        assert_eq!(
            large.direct_parent(64),
            DirectParent::Indirect {
                indirect: 0,
                slot: 0
            }
        );
        assert_eq!(
            large.direct_parent(64 + 1008 * 6 + 3),
            DirectParent::Indirect {
                indirect: 6,
                slot: 3
            }
        );
        assert_eq!(large.table_children(585), 262_143 - 585 * 448);
        assert!(MAX_MAPPING_PAGES * ENTRIES_PER_PAGE as u64 >= MAX_VOLUME_BLOCKS);
    }

    #[test]
    fn geometry_rejects_bounds_and_growth_is_monotonic() {
        for (volume, backing, segment) in [
            (0, 100, 4),
            ((1 << 31) + 1, 100, 4),
            (1, MAX_BACKING_BLOCKS + 1, 4),
            (1, 100, 3),
            (1, 2 + 2 * 4, 4),
            (1, 2 + 3 * 4 - 1, 4),
            (1, 2 + (MAX_SEGMENTS + 1) * 4, 4),
        ] {
            assert!(
                Geometry::new(volume, backing, segment).is_err(),
                "{volume} {backing} {segment}"
            );
        }
        let geometry = Geometry::new(1, 2 + 3 * 4, 4).unwrap();
        assert_eq!(geometry.with_backing(2 + 5 * 4).unwrap().segment_count, 5);
        assert_eq!(
            geometry.with_backing(2 + 3 * 4 + 1).unwrap().segment_count,
            3
        );
        assert!(geometry.with_backing(2 + 2 * 4).is_err());
        assert_eq!(Geometry::default_blocks_per_segment(1 << 30), 2048);
        assert_eq!(Geometry::default_blocks_per_segment(2 << 30), 2048);
        assert_eq!(Geometry::default_blocks_per_segment((2 << 30) + 1), 4096);
        assert_eq!(Geometry::default_blocks_per_segment(4 << 40), 4096);
        assert_eq!(Geometry::default_blocks_per_segment(7 << 40), 8192);
        assert_eq!(Geometry::default_blocks_per_segment(20 << 40), 8192);
    }
}
