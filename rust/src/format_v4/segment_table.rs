use super::geometry::TABLE_ENTRIES;
use super::{
    BODY_END, Geometry, HeadId, RecordHeader, RecordKind, decode_metadata_record, require_zero,
    seal_metadata_record,
};
use crate::{BLOCK_SIZE, invalid_data, read_u32, read_u64};
use std::io;

const PAGE_ID_OFFSET: usize = 32;
const SEAL_LSN_OFFSET: usize = 40;
const FLAGS_OFFSET: usize = SEAL_LSN_OFFSET + TABLE_ENTRIES * 8;
const STATE_MASK: u8 = 0x07;
const RESERVED_MASK: u8 = 0x78;
const HEAD_MASK: u8 = 0x80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SegmentState {
    Free = 0,
    Open = 1,
    Sealed = 2,
    PendingFree = 3,
}

impl SegmentState {
    fn from_bits(bits: u8) -> io::Result<Self> {
        match bits {
            0 => Ok(Self::Free),
            1 => Ok(Self::Open),
            2 => Ok(Self::Sealed),
            3 => Ok(Self::PendingFree),
            _ => Err(invalid_data("unknown segment state")),
        }
    }

    pub(crate) const fn has_seal(self) -> bool {
        matches!(self, Self::Sealed | Self::PendingFree)
    }
}

/// The decoded persistent fields of one segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentEntry {
    pub(crate) state: SegmentState,
    /// Present only while `Open`.
    pub(crate) head: Option<HeadId>,
    /// The closing `SegmentLink` LSN; zero while `Free` or `Open`.
    pub(crate) seal_lsn: u64,
}

impl SegmentEntry {
    pub(crate) const FREE: Self = Self {
        state: SegmentState::Free,
        head: None,
        seal_lsn: 0,
    };

    pub(crate) fn open(head: HeadId) -> Self {
        Self {
            state: SegmentState::Open,
            head: Some(head),
            seal_lsn: 0,
        }
    }

    pub(crate) fn sealed(seal_lsn: u64) -> Self {
        Self {
            state: SegmentState::Sealed,
            head: None,
            seal_lsn,
        }
    }

    pub(crate) fn pending_free(seal_lsn: u64) -> Self {
        Self {
            state: SegmentState::PendingFree,
            head: None,
            seal_lsn,
        }
    }

    pub(crate) fn validate(self) -> io::Result<()> {
        if self.head.is_some() != (self.state == SegmentState::Open) {
            return Err(invalid_data("owning head is defined only while Open"));
        }
        if (self.seal_lsn != 0) != self.state.has_seal() {
            return Err(invalid_data("seal LSN must match the segment state"));
        }
        Ok(())
    }

    pub(crate) fn flags(self) -> u8 {
        self.state as u8 | self.head.map_or(0, |head| (head as u8) << 7)
    }

    pub(crate) fn from_fields(flags: u8, seal_lsn: u64) -> io::Result<Self> {
        if flags & RESERVED_MASK != 0 {
            return Err(invalid_data("reserved segment flag bits must be zero"));
        }
        let state = SegmentState::from_bits(flags & STATE_MASK)?;
        let head_bit = flags & HEAD_MASK != 0;
        let head = match (state, head_bit) {
            (SegmentState::Open, false) => Some(HeadId::User),
            (SegmentState::Open, true) => Some(HeadId::Relocation),
            (_, false) => None,
            (_, true) => return Err(invalid_data("head bit set outside Open")),
        };
        let entry = Self {
            state,
            head,
            seal_lsn,
        };
        entry.validate()?;
        Ok(entry)
    }
}

/// The persistent state of 448 consecutive segments as parallel arrays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SegmentTablePage {
    pub(crate) header: RecordHeader,
    pub(crate) page_id: u32,
    pub(crate) seal_lsn: [u64; TABLE_ENTRIES],
    pub(crate) flags: [u8; TABLE_ENTRIES],
}

pub(crate) struct TablePageExpectation {
    pub(crate) geometry: Geometry,
    pub(crate) volume_id: u64,
    pub(crate) page_id: u32,
    pub(crate) boundary: u32,
    pub(crate) maximum_lsn: u64,
}

impl SegmentTablePage {
    pub(crate) fn empty(header: RecordHeader, page_id: u32) -> Self {
        Self {
            header,
            page_id,
            seal_lsn: [0; TABLE_ENTRIES],
            flags: [0; TABLE_ENTRIES],
        }
    }

    pub(crate) fn set(&mut self, slot: usize, entry: SegmentEntry) {
        self.seal_lsn[slot] = entry.seal_lsn;
        self.flags[slot] = entry.flags();
    }

    pub(crate) fn entry(&self, slot: usize) -> io::Result<SegmentEntry> {
        SegmentEntry::from_fields(self.flags[slot], self.seal_lsn[slot])
    }

    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::SegmentTablePage {
            return Err(invalid_data("expected segment table page"));
        }
        self.header.validate(geometry, self.header.volume_id)?;
        if self.page_id >= geometry.table_pages {
            return Err(invalid_data("segment table page ID out of range"));
        }
        let used = geometry.table_children(self.page_id);
        for slot in 0..TABLE_ENTRIES {
            if slot >= used {
                if self.flags[slot] != 0 || self.seal_lsn[slot] != 0 {
                    return Err(invalid_data("segment table entry beyond the last segment"));
                }
                continue;
            }
            let entry = self.entry(slot)?;
            if entry.seal_lsn > self.header.lsn {
                return Err(invalid_data("seal LSN newer than its table page"));
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
        for slot in 0..TABLE_ENTRIES {
            let offset = SEAL_LSN_OFFSET + slot * 8;
            bytes[offset..offset + 8].copy_from_slice(&self.seal_lsn[slot].to_le_bytes());
            bytes[FLAGS_OFFSET + slot] = self.flags[slot];
        }
        seal_metadata_record(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], expected: &TablePageExpectation) -> io::Result<Self> {
        let (header, bytes) = decode_metadata_record(bytes, expected.geometry, expected.volume_id)?;
        if header.kind != RecordKind::SegmentTablePage
            || header.boundary != expected.boundary
            || header.lsn > expected.maximum_lsn
            || read_u32(bytes, PAGE_ID_OFFSET) != expected.page_id
        {
            return Err(invalid_data("segment table page identity mismatch"));
        }
        require_zero(&bytes[36..40], "segment table page padding")?;
        require_zero(
            &bytes[FLAGS_OFFSET + TABLE_ENTRIES..BODY_END],
            "segment table page padding",
        )?;
        let mut page = Self::empty(header, expected.page_id);
        for slot in 0..TABLE_ENTRIES {
            page.seal_lsn[slot] = read_u64(bytes, SEAL_LSN_OFFSET + slot * 8);
            page.flags[slot] = bytes[FLAGS_OFFSET + slot];
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
        // 500 segments of 8 blocks: two table pages, the second holding 52 entries.
        Geometry::new(4096, 2 + 500 * 8, 8).unwrap()
    }

    fn fixture(page_id: u32) -> (SegmentTablePage, TablePageExpectation) {
        let geometry = geometry();
        let header = RecordHeader {
            kind: RecordKind::SegmentTablePage,
            volume_id: 21,
            lsn: 90,
            previous_boundary: NO_PREVIOUS_BOUNDARY,
            boundary: 2 + 8 * 3,
        };
        let mut page = SegmentTablePage::empty(header, page_id);
        page.set(0, SegmentEntry::open(HeadId::User));
        page.set(1, SegmentEntry::open(HeadId::Relocation));
        page.set(2, SegmentEntry::sealed(17));
        page.set(3, SegmentEntry::pending_free(40));
        let expected = TablePageExpectation {
            geometry,
            volume_id: 21,
            page_id,
            boundary: header.boundary,
            maximum_lsn: 90,
        };
        (page, expected)
    }

    #[test]
    fn flags_pack_state_and_head_exactly_and_reject_invalid_bits() {
        assert_eq!(SegmentEntry::FREE.flags(), 0x00);
        assert_eq!(SegmentEntry::open(HeadId::User).flags(), 0x01);
        assert_eq!(SegmentEntry::open(HeadId::Relocation).flags(), 0x81);
        assert_eq!(SegmentEntry::sealed(5).flags(), 0x02);
        assert_eq!(SegmentEntry::pending_free(5).flags(), 0x03);
        for flags in 0..=u8::MAX {
            for seal in [0, 9] {
                let decoded = SegmentEntry::from_fields(flags, seal);
                let state = flags & 0x07;
                let valid = flags & 0x78 == 0
                    && state <= 3
                    && (flags & 0x80 == 0 || state == 1)
                    && ((seal != 0) == (state >= 2));
                assert_eq!(decoded.is_ok(), valid, "flags={flags:#04x} seal={seal}");
                if let Ok(entry) = decoded {
                    assert_eq!(entry.flags(), flags);
                    assert_eq!(entry.seal_lsn, seal);
                }
            }
        }
    }

    #[test]
    fn table_page_has_exact_parallel_arrays_and_round_trips() {
        let (page, expected) = fixture(0);
        let mut bytes = [0xff; BLOCK_SIZE];
        page.encode(expected.geometry, &mut bytes).unwrap();
        assert_eq!(bytes[5], 3);
        assert_eq!(&bytes[32..40], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read_u64(&bytes, 40 + 2 * 8), 17);
        assert_eq!(read_u64(&bytes, 40 + 3 * 8), 40);
        assert!(bytes[40 + 4 * 8..3624].iter().all(|byte| *byte == 0));
        assert_eq!(&bytes[3624..3628], &[0x01, 0x81, 0x02, 0x03]);
        assert!(bytes[3628..4072].iter().all(|byte| *byte == 0));
        assert_eq!(
            read_u64(&bytes, 4072),
            xxhash_rust::xxh3::xxh3_64(&bytes[..40])
        );
        assert!(bytes[4080..4088].iter().all(|byte| *byte == 0));
        assert_eq!(SegmentTablePage::decode(&bytes, &expected).unwrap(), page);
        assert_eq!(page.entry(3).unwrap(), SegmentEntry::pending_free(40));
    }

    #[test]
    fn last_partial_page_and_boundary_entries_round_trip() {
        let (mut page, mut expected) = fixture(1);
        page.set(51, SegmentEntry::sealed(89));
        let mut bytes = [0; BLOCK_SIZE];
        page.encode(expected.geometry, &mut bytes).unwrap();
        assert_eq!(read_u64(&bytes, 40 + 51 * 8), 89);
        assert_eq!(bytes[3624 + 51], 0x02);
        assert_eq!(SegmentTablePage::decode(&bytes, &expected).unwrap(), page);
        page.set(52, SegmentEntry::sealed(89));
        assert!(page.encode(expected.geometry, &mut bytes).is_err());
        page.set(52, SegmentEntry::FREE);
        page.set(447, SegmentEntry::sealed(1));
        assert!(page.encode(expected.geometry, &mut bytes).is_err());
        expected.page_id = 2;
        page.page_id = 2;
        page.set(447, SegmentEntry::FREE);
        assert!(page.encode(expected.geometry, &mut bytes).is_err());
    }

    #[test]
    fn table_page_rejects_malformed_entries_identity_and_padding() {
        let (page, expected) = fixture(0);
        let mut original = [0; BLOCK_SIZE];
        page.encode(expected.geometry, &mut original).unwrap();
        for (offset, value) in [
            (5, 2),
            (32, 1),
            (36, 1),
            (40, 1),
            (40 + 2 * 8, 0),
            (40 + 2 * 8, 91),
            (3624, 0x04),
            (3624, 0x08),
            (3624 + 2, 0x82),
            (3624 + 4, 0x02),
            (4080, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            crate::format_v4::test_support::reseal_metadata(&mut bytes, offset);
            assert!(
                SegmentTablePage::decode(&bytes, &expected).is_err(),
                "offset={offset} value={value}"
            );
        }
        let mut wrong = TablePageExpectation {
            geometry: expected.geometry,
            volume_id: expected.volume_id,
            page_id: 0,
            boundary: expected.boundary,
            maximum_lsn: 89,
        };
        assert!(SegmentTablePage::decode(&original, &wrong).is_err());
        wrong.maximum_lsn = 90;
        wrong.boundary += 1;
        assert!(SegmentTablePage::decode(&original, &wrong).is_err());
        wrong.boundary -= 1;
        wrong.volume_id = 22;
        assert!(SegmentTablePage::decode(&original, &wrong).is_err());
        for length in [0, 40, 4095] {
            assert!(SegmentTablePage::decode(&original[..length], &expected).is_err());
        }
        original[3000] ^= 1;
        assert!(SegmentTablePage::decode(&original, &expected).is_err());
    }
}
