use super::{
    Geometry, RecordHeader, RecordKind, decode_metadata_record, require_zero, seal_metadata_record,
};
use crate::mapping_cache::{ENTRIES_PER_PAGE, MappingEntries};
use crate::{BLOCK_SIZE, invalid_data, read_u32, read_u64};
use std::io;

// ADR-07's explicit offsets, unchanged in format 4; native MappingEntries is never cast to bytes.
const PAGE_ID_OFFSET: usize = 32;
const ADDRESS_OFFSET: usize = 40;
const PAYLOAD_CHECKSUM_OFFSET: usize = 1384;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MappingPageRecord {
    pub(crate) header: RecordHeader,
    pub(crate) page_id: u32,
    pub(crate) entries: MappingEntries,
}

pub(crate) struct PageExpectation {
    pub(crate) geometry: Geometry,
    pub(crate) volume_id: u64,
    pub(crate) page_id: u32,
    pub(crate) boundary: u32,
    pub(crate) maximum_lsn: u64,
}

/// Validates entry arrays against the volume geometry. Format 4 addresses can precede or
/// follow the snapshot because segments are reused; every nonzero address lies in a segment.
pub(crate) fn validate_entries(
    entries: &MappingEntries,
    geometry: Geometry,
    page_id: u32,
) -> io::Result<()> {
    let valid_entries = geometry.page_entries(page_id)?;
    for index in 0..ENTRIES_PER_PAGE {
        let physical = entries.physical_addr[index];
        let checksum = entries.payload_checksum[index];
        if (physical == 0 && checksum != 0)
            || (index >= valid_entries && (physical != 0 || checksum != 0))
            || (physical != 0 && (physical & (1 << 31) != 0 || !geometry.contains_block(physical)))
        {
            return Err(invalid_data("invalid mapping-page entry"));
        }
    }
    Ok(())
}

impl MappingPageRecord {
    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::MappingPage {
            return Err(invalid_data("expected mapping-page record"));
        }
        self.header.validate(geometry, self.header.volume_id)?;
        validate_entries(&self.entries, geometry, self.page_id)
    }

    pub(crate) fn encode(
        &self,
        geometry: Geometry,
        bytes: &mut [u8; BLOCK_SIZE],
    ) -> io::Result<()> {
        self.validate(geometry)?;
        self.header.encode(bytes);
        bytes[PAGE_ID_OFFSET..PAGE_ID_OFFSET + 4].copy_from_slice(&self.page_id.to_le_bytes());
        for index in 0..ENTRIES_PER_PAGE {
            let address = ADDRESS_OFFSET + index * 4;
            let checksum = PAYLOAD_CHECKSUM_OFFSET + index * 8;
            bytes[address..address + 4]
                .copy_from_slice(&self.entries.physical_addr[index].to_le_bytes());
            bytes[checksum..checksum + 8]
                .copy_from_slice(&self.entries.payload_checksum[index].to_le_bytes());
        }
        seal_metadata_record(bytes);
        Ok(())
    }

    /// Validates traversal fields and the page ID without accepting the mapping entries.
    #[cfg(test)]
    pub(crate) fn decode_framing(
        bytes: &[u8],
        geometry: Geometry,
        volume_id: u64,
    ) -> io::Result<(RecordHeader, u32)> {
        let header = super::decode_metadata_framing(bytes, geometry, volume_id)?;
        if header.kind != RecordKind::MappingPage {
            return Err(invalid_data("expected mapping-page framing"));
        }
        let page_id = read_u32(bytes, PAGE_ID_OFFSET);
        geometry.page_entries(page_id)?;
        require_zero(&bytes[36..40], "mapping page padding")?;
        Ok((header, page_id))
    }

    pub(crate) fn decode(bytes: &[u8], expected: &PageExpectation) -> io::Result<Self> {
        let (header, bytes) = decode_metadata_record(bytes, expected.geometry, expected.volume_id)?;
        if header.kind != RecordKind::MappingPage
            || header.boundary != expected.boundary
            || header.lsn > expected.maximum_lsn
            || read_u32(bytes, PAGE_ID_OFFSET) != expected.page_id
        {
            return Err(invalid_data("mapping-page identity or LSN mismatch"));
        }
        require_zero(&bytes[36..40], "mapping page padding")?;
        let mut entries = MappingEntries::zeroed();
        for index in 0..ENTRIES_PER_PAGE {
            entries.physical_addr[index] = read_u32(bytes, ADDRESS_OFFSET + index * 4);
            entries.payload_checksum[index] = read_u64(bytes, PAYLOAD_CHECKSUM_OFFSET + index * 8);
        }
        let record = Self {
            header,
            page_id: expected.page_id,
            entries,
        };
        record.validate(expected.geometry)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v4::NO_PREVIOUS_BOUNDARY;

    fn fixture() -> (MappingPageRecord, PageExpectation) {
        let geometry = Geometry::new(673, 2 + 4 * 256, 256).unwrap();
        let mut entries = MappingEntries::zeroed();
        // Format 4 lets a snapshot point behind or ahead of itself.
        entries.physical_addr[0] = 2;
        entries.payload_checksum[0] = 0x0102_0304_0506_0708;
        entries.physical_addr[1] = 2 + 4 * 256 - 1;
        entries.payload_checksum[1] = 9;
        let record = MappingPageRecord {
            header: RecordHeader {
                kind: RecordKind::MappingPage,
                volume_id: 17,
                lsn: 3,
                previous_boundary: NO_PREVIOUS_BOUNDARY,
                boundary: 2 + 256,
            },
            page_id: 1,
            entries,
        };
        let expected = PageExpectation {
            geometry,
            volume_id: 17,
            page_id: 1,
            boundary: 2 + 256,
            maximum_lsn: 3,
        };
        (record, expected)
    }

    #[test]
    fn mapping_snapshot_keeps_the_format_3_layout_with_a_format_4_header() {
        let (record, expected) = fixture();
        let mut bytes = [0xff; BLOCK_SIZE];
        record.encode(expected.geometry, &mut bytes).unwrap();
        assert_eq!(bytes[4], 4);
        assert_eq!(bytes[5], 2);
        assert_eq!(&bytes[32..40], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[40..44], &[2, 0, 0, 0]);
        assert_eq!(read_u32(&bytes, 44), 2 + 4 * 256 - 1);
        assert!(bytes[48..1384].iter().all(|byte| *byte == 0));
        assert_eq!(&bytes[1384..1392], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(read_u64(&bytes, 1392), 9);
        assert!(bytes[1400..4072].iter().all(|byte| *byte == 0));
        assert_eq!(
            read_u64(&bytes, 4072),
            xxhash_rust::xxh3::xxh3_64(&bytes[..40])
        );
        assert!(bytes[4080..4088].iter().all(|byte| *byte == 0));
        assert_eq!(
            MappingPageRecord::decode(&bytes, &expected).unwrap(),
            record
        );
        assert_eq!(
            MappingPageRecord::decode_framing(&bytes, expected.geometry, 17).unwrap(),
            (record.header, 1)
        );
    }

    #[test]
    fn rejects_malformed_snapshots_and_reserved_address_bit() {
        let (record, expected) = fixture();
        let mut original = [0; BLOCK_SIZE];
        record.encode(expected.geometry, &mut original).unwrap();
        for (offset, value) in [
            (5, 1),
            (8, 18),
            (16, 4),
            (24, 1),
            (28, 10),
            (32, 2),
            (36, 1),
            (4080, 1),
            (40, 0),
            (40, 1),
            (43, 0x80),
            (1400, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            crate::format_v4::test_support::reseal_metadata(&mut bytes, offset);
            assert!(
                MappingPageRecord::decode(&bytes, &expected).is_err(),
                "offset={offset} value={value}"
            );
        }
        let mut beyond = original;
        beyond[40..44].copy_from_slice(&(2 + 4 * 256_u32).to_le_bytes());
        seal_metadata_record(&mut beyond);
        assert!(MappingPageRecord::decode(&beyond, &expected).is_err());
        for length in [0, 35, 4072, 4095] {
            assert!(MappingPageRecord::decode(&original[..length], &expected).is_err());
        }
        original[100] ^= 1;
        assert!(MappingPageRecord::decode(&original, &expected).is_err());
        // Partial last page (673 = 2 * 336 + 1): only entry 0 may be mapped.
        let (mut record, mut expected) = fixture();
        record.page_id = 2;
        expected.page_id = 2;
        record.entries.physical_addr[1] = 0;
        record.entries.payload_checksum[1] = 0;
        record.encode(expected.geometry, &mut original).unwrap();
        assert_eq!(
            MappingPageRecord::decode(&original, &expected).unwrap(),
            record
        );
        record.entries.physical_addr[1] = 2;
        record.entries.payload_checksum[1] = 1;
        assert!(record.encode(expected.geometry, &mut original).is_err());
    }

    #[test]
    fn mapping_framing_survives_only_body_damage() {
        let (record, expected) = fixture();
        let mut original = [0; BLOCK_SIZE];
        record.encode(expected.geometry, &mut original).unwrap();
        for offset in 0..BLOCK_SIZE {
            let mut damaged = original;
            damaged[offset] ^= 1;
            let framing = MappingPageRecord::decode_framing(&damaged, expected.geometry, 17);
            if offset < 40 || (4072..4080).contains(&offset) {
                assert!(framing.is_err(), "framing byte {offset}");
            } else {
                assert_eq!(framing.unwrap(), (record.header, 1), "body byte {offset}");
            }
            assert!(MappingPageRecord::decode(&damaged, &expected).is_err());
        }
    }
}
