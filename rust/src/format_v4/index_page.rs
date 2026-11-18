use super::geometry::INDEX_CHILDREN;
use super::{
    BODY_END, Geometry, RecordHeader, RecordKind, decode_metadata_record, require_zero,
    seal_metadata_record,
};
use crate::{BLOCK_SIZE, invalid_data, read_u32};
use std::io;

const PAGE_ID_OFFSET: usize = 32;
const LEVEL_OFFSET: usize = 36;
const CHILDREN_OFFSET: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum MappingIndexLevel {
    /// Children are `MappingPage` records.
    Direct = 1,
    /// Children are Direct `MappingIndexPage` records.
    Indirect = 2,
}

impl TryFrom<u8> for MappingIndexLevel {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Direct),
            2 => Ok(Self::Indirect),
            _ => Err(invalid_data("unknown mapping index level")),
        }
    }
}

/// One page of the mapping index tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MappingIndexPage {
    pub(crate) header: RecordHeader,
    pub(crate) page_id: u32,
    pub(crate) level: MappingIndexLevel,
    pub(crate) child_blocks: [u32; INDEX_CHILDREN],
}

pub(crate) struct IndexPageExpectation {
    pub(crate) geometry: Geometry,
    pub(crate) volume_id: u64,
    pub(crate) page_id: u32,
    pub(crate) level: MappingIndexLevel,
    pub(crate) boundary: u32,
    pub(crate) maximum_lsn: u64,
}

impl MappingIndexPage {
    pub(crate) fn empty(header: RecordHeader, page_id: u32, level: MappingIndexLevel) -> Self {
        Self {
            header,
            page_id,
            level,
            child_blocks: [0; INDEX_CHILDREN],
        }
    }

    fn used_children(&self, geometry: Geometry) -> io::Result<usize> {
        let (pages, used) = match self.level {
            MappingIndexLevel::Direct => (
                geometry.direct_pages,
                geometry.direct_children(self.page_id),
            ),
            MappingIndexLevel::Indirect => (
                geometry.indirect_pages,
                geometry.indirect_children(self.page_id),
            ),
        };
        if self.page_id >= pages {
            return Err(invalid_data("mapping index page ID out of range"));
        }
        Ok(used)
    }

    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::MappingIndexPage {
            return Err(invalid_data("expected mapping index page"));
        }
        self.header.validate(geometry, self.header.volume_id)?;
        let used = self.used_children(geometry)?;
        for (index, &child) in self.child_blocks.iter().enumerate() {
            if index >= used && child != 0 {
                return Err(invalid_data("index child beyond the volume's shape"));
            }
            if child != 0 && !geometry.contains_block(child) {
                return Err(invalid_data("index child outside every segment"));
            }
        }
        Ok(())
    }

    pub(crate) fn encode(
        &self,
        geometry: Geometry,
        bytes: &mut [u8; BLOCK_SIZE],
    ) -> io::Result<()> {
        self.validate(geometry)?;
        self.header.encode(bytes);
        bytes[PAGE_ID_OFFSET..PAGE_ID_OFFSET + 4].copy_from_slice(&self.page_id.to_le_bytes());
        bytes[LEVEL_OFFSET] = self.level as u8;
        for (child, encoded) in self
            .child_blocks
            .iter()
            .zip(bytes[CHILDREN_OFFSET..BODY_END].chunks_exact_mut(4))
        {
            encoded.copy_from_slice(&child.to_le_bytes());
        }
        seal_metadata_record(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], expected: &IndexPageExpectation) -> io::Result<Self> {
        let (header, bytes) = decode_metadata_record(bytes, expected.geometry, expected.volume_id)?;
        if header.kind != RecordKind::MappingIndexPage
            || header.boundary != expected.boundary
            || header.lsn > expected.maximum_lsn
            || read_u32(bytes, PAGE_ID_OFFSET) != expected.page_id
            || MappingIndexLevel::try_from(bytes[LEVEL_OFFSET])? != expected.level
        {
            return Err(invalid_data("mapping index page identity mismatch"));
        }
        require_zero(&bytes[37..40], "mapping index page padding")?;
        let mut page = Self::empty(header, expected.page_id, expected.level);
        for (child, encoded) in page
            .child_blocks
            .iter_mut()
            .zip(bytes[CHILDREN_OFFSET..BODY_END].chunks_exact(4))
        {
            *child = u32::from_le_bytes(encoded.try_into().expect("four-byte chunk"));
        }
        page.validate(expected.geometry)?;
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v4::NO_PREVIOUS_BOUNDARY;

    fn geometry() -> Geometry {
        // 100 GiB logical: 78,020 mapping pages, 78 direct pages, one indirect page.
        Geometry::new(26_214_400, 2 + 100 * 2048, 2048).unwrap()
    }

    fn fixture(level: MappingIndexLevel, page_id: u32) -> (MappingIndexPage, IndexPageExpectation) {
        let geometry = geometry();
        let header = RecordHeader {
            kind: RecordKind::MappingIndexPage,
            volume_id: 23,
            lsn: 300,
            previous_boundary: NO_PREVIOUS_BOUNDARY,
            boundary: 2 + 2048 * 4,
        };
        let mut page = MappingIndexPage::empty(header, page_id, level);
        page.child_blocks[0] = 2;
        page.child_blocks[1] = 2 + 2048 * 100 - 1;
        (
            page,
            IndexPageExpectation {
                geometry,
                volume_id: 23,
                page_id,
                level,
                boundary: header.boundary,
                maximum_lsn: 300,
            },
        )
    }

    #[test]
    fn index_page_has_exact_layout_for_both_levels() {
        let geometry = geometry();
        assert_eq!(
            (
                geometry.page_count,
                geometry.direct_pages,
                geometry.indirect_pages
            ),
            (78_020, 78, 1)
        );
        for level in [MappingIndexLevel::Direct, MappingIndexLevel::Indirect] {
            let (mut page, expected) = fixture(level, 0);
            let last = if level == MappingIndexLevel::Direct {
                1007
            } else {
                78 - 64 - 1
            };
            page.child_blocks[last] = 4;
            let mut bytes = [0xff; BLOCK_SIZE];
            page.encode(geometry, &mut bytes).unwrap();
            assert_eq!(bytes[5], 8);
            assert_eq!(read_u32(&bytes, 32), 0);
            assert_eq!(bytes[36], level as u8);
            assert_eq!(&bytes[37..40], &[0, 0, 0]);
            assert_eq!(read_u32(&bytes, 40), 2);
            assert_eq!(read_u32(&bytes, 44), 2 + 2048 * 100 - 1);
            assert_eq!(read_u32(&bytes, 40 + last * 4), 4);
            assert_eq!(
                read_u64_at(&bytes, 4072),
                xxhash_rust::xxh3::xxh3_64(&bytes[..40])
            );
            assert_eq!(MappingIndexPage::decode(&bytes, &expected).unwrap(), page);
            if level == MappingIndexLevel::Indirect {
                page.child_blocks[last + 1] = 4;
                assert!(page.encode(geometry, &mut bytes).is_err());
            }
        }
        // The last direct page covers 78,020 - 77 * 1008 = 404 mapping pages.
        let (mut page, expected) = fixture(MappingIndexLevel::Direct, 77);
        page.child_blocks[403] = 4;
        let mut bytes = [0; BLOCK_SIZE];
        page.encode(geometry, &mut bytes).unwrap();
        assert_eq!(MappingIndexPage::decode(&bytes, &expected).unwrap(), page);
        page.child_blocks[404] = 4;
        assert!(page.encode(geometry, &mut bytes).is_err());
        page.child_blocks[404] = 0;
        page.page_id = 78;
        assert!(page.encode(geometry, &mut bytes).is_err());
    }

    fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
        crate::read_u64(bytes, offset)
    }

    #[test]
    fn index_page_rejects_malformed_children_levels_and_identity() {
        let (page, expected) = fixture(MappingIndexLevel::Direct, 3);
        let mut original = [0; BLOCK_SIZE];
        page.encode(expected.geometry, &mut original).unwrap();
        for (offset, value) in [
            (5, 2),
            (32, 4),
            (36, 0),
            (36, 2),
            (36, 3),
            (37, 1),
            (40, 1),
            (4080, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            crate::format_v4::test_support::reseal_metadata(&mut bytes, offset);
            assert!(
                MappingIndexPage::decode(&bytes, &expected).is_err(),
                "offset={offset} value={value}"
            );
        }
        let mut bytes = original;
        bytes[40..44].copy_from_slice(&(2 + 2048 * 100_u32).to_le_bytes());
        seal_metadata_record(&mut bytes);
        assert!(MappingIndexPage::decode(&bytes, &expected).is_err());
        let mut stale = IndexPageExpectation {
            geometry: expected.geometry,
            volume_id: 23,
            page_id: 3,
            level: MappingIndexLevel::Direct,
            boundary: expected.boundary,
            maximum_lsn: 299,
        };
        assert!(MappingIndexPage::decode(&original, &stale).is_err());
        stale.maximum_lsn = 300;
        stale.boundary += 1;
        assert!(MappingIndexPage::decode(&original, &stale).is_err());
        for length in [0, 40, 4095] {
            assert!(MappingIndexPage::decode(&original[..length], &expected).is_err());
        }
        original[2000] ^= 1;
        assert!(MappingIndexPage::decode(&original, &expected).is_err());
    }
}
