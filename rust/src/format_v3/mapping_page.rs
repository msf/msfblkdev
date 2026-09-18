use super::{
    RECORD_MAGIC, RecordHeader, RecordKind, geometry::Geometry, seal_metadata, validate_metadata,
};
use crate::mapping_cache::{ENTRIES_PER_PAGE, MappingEntries};
use crate::{BLOCK_SIZE, invalid_data, read_u32, read_u64};
use std::io;
use xxhash_rust::xxh3::xxh3_64;

// ADR-07's explicit on-disk offsets; native MappingEntries is never cast to bytes.
const ADDRESS_OFFSET: usize = 40;
const PAYLOAD_CHECKSUM_OFFSET: usize = 1320;
const HEADER_CHECKSUM_OFFSET: usize = 3880;
const PADDING_OFFSET: usize = 3888;

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

impl MappingPageRecord {
    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::MappingPage {
            return Err(invalid_data("expected mapping-page record"));
        }
        self.header.validate_location(
            self.header.volume_id,
            geometry.log_start,
            geometry.backing_blocks,
        )?;
        let valid_entries = geometry.page_entries(self.page_id)?;
        for index in 0..ENTRIES_PER_PAGE {
            let physical = self.entries.physical_addr[index];
            let checksum = self.entries.payload_checksum[index];
            if (physical == 0 && checksum != 0)
                || (index >= valid_entries && (physical != 0 || checksum != 0))
                || (physical != 0
                    && (physical < geometry.log_start || physical >= self.header.boundary))
            {
                return Err(invalid_data("invalid mapping-page entry"));
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
        self.header.encode(bytes)?;
        bytes[32..36].copy_from_slice(&self.page_id.to_le_bytes());
        for index in 0..ENTRIES_PER_PAGE {
            let address = ADDRESS_OFFSET + index * 4;
            let checksum = PAYLOAD_CHECKSUM_OFFSET + index * 8;
            bytes[address..address + 4]
                .copy_from_slice(&self.entries.physical_addr[index].to_le_bytes());
            bytes[checksum..checksum + 8]
                .copy_from_slice(&self.entries.payload_checksum[index].to_le_bytes());
        }
        let header_checksum = xxh3_64(&bytes[..ADDRESS_OFFSET]);
        bytes[HEADER_CHECKSUM_OFFSET..PADDING_OFFSET]
            .copy_from_slice(&header_checksum.to_le_bytes());
        seal_metadata(bytes);
        Ok(())
    }

    /// Validate traversal fields without accepting the snapshot's mapping entries.
    pub(crate) fn decode_framing(
        bytes: &[u8],
        geometry: Geometry,
        volume_id: u64,
    ) -> io::Result<RecordHeader> {
        let bytes: &[u8; BLOCK_SIZE] = bytes
            .try_into()
            .map_err(|_| invalid_data("mapping record length"))?;
        if read_u64(bytes, HEADER_CHECKSUM_OFFSET) != xxh3_64(&bytes[..ADDRESS_OFFSET])
            || bytes[36..40] != [0; 4]
        {
            return Err(invalid_data("mapping header checksum or padding mismatch"));
        }
        let header = RecordHeader::decode_fields(bytes)?;
        if header.kind != RecordKind::MappingPage {
            return Err(invalid_data("expected mapping-page framing"));
        }
        header.validate_location(volume_id, geometry.log_start, geometry.backing_blocks)?;
        geometry.page_entries(read_u32(bytes, 32))?;
        Ok(header)
    }

    pub(crate) fn decode(bytes: &[u8], expected: &PageExpectation) -> io::Result<Self> {
        let header = Self::decode_framing(bytes, expected.geometry, expected.volume_id)?;
        let bytes = validate_metadata(bytes, RECORD_MAGIC)?;
        if header.volume_id != expected.volume_id
            || header.boundary != expected.boundary
            || header.lsn > expected.maximum_lsn
            || read_u32(bytes, 32) != expected.page_id
            || bytes[36..40]
                .iter()
                .chain(&bytes[PADDING_OFFSET..super::CHECKSUM_OFFSET])
                .any(|byte| *byte != 0)
        {
            return Err(invalid_data(
                "mapping-page identity, generation, or padding mismatch",
            ));
        }
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

    fn fixture() -> (MappingPageRecord, PageExpectation) {
        let geometry = Geometry::new(641, 1000).unwrap();
        let mut entries = MappingEntries::zeroed();
        entries.physical_addr[0] = 4;
        entries.payload_checksum[0] = 0x0102_0304_0506_0708;
        let record = MappingPageRecord {
            header: RecordHeader {
                kind: RecordKind::MappingPage,
                volume_id: 17,
                lsn: 3,
                previous_boundary: 8,
                boundary: 9,
            },
            page_id: 2,
            entries,
        };
        let expected = PageExpectation {
            geometry,
            volume_id: 17,
            page_id: 2,
            boundary: 9,
            maximum_lsn: 3,
        };
        (record, expected)
    }

    #[test]
    fn mapping_snapshot_has_exact_arrays_padding_and_checksum() {
        let (record, expected) = fixture();
        let mut bytes = [0xff; BLOCK_SIZE];
        record.encode(expected.geometry, &mut bytes).unwrap();
        assert_eq!(bytes[5], 2);
        assert_eq!(&bytes[32..40], &[2, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[40..44], &[4, 0, 0, 0]);
        assert!(bytes[44..1320].iter().all(|byte| *byte == 0));
        assert_eq!(&bytes[1320..1328], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert!(bytes[1328..3880].iter().all(|byte| *byte == 0));
        assert_eq!(read_u64(&bytes, 3880), xxh3_64(&bytes[..40]));
        assert!(bytes[3888..4088].iter().all(|byte| *byte == 0));
        let checksum = read_u64(&bytes, 4088);
        let mut copy = bytes;
        copy[4088..].fill(0);
        assert_eq!(checksum, xxhash_rust::xxh3::xxh3_64(&copy));
        assert_eq!(
            MappingPageRecord::decode(&bytes, &expected).unwrap(),
            record
        );
        assert_eq!(bytes[4088..], checksum.to_le_bytes());
    }

    #[test]
    fn rejects_malformed_mapping_snapshots_without_panic_or_mutation() {
        let (record, expected) = fixture();
        let mut original = [0; BLOCK_SIZE];
        record.encode(expected.geometry, &mut original).unwrap();
        for (offset, value) in [
            (5, 1),
            (8, 18),
            (16, 4),
            (24, 7),
            (28, 10),
            (32, 1),
            (36, 1),
            (3880, original[3880] ^ 1),
            (3888, 1),
            (4087, 1),
            (40, 0),
            (40, 3),
            (40, 9),
            (44, 4),
            (1328, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            if offset < ADDRESS_OFFSET {
                let checksum = xxh3_64(&bytes[..ADDRESS_OFFSET]);
                bytes[HEADER_CHECKSUM_OFFSET..PADDING_OFFSET]
                    .copy_from_slice(&checksum.to_le_bytes());
            }
            seal_metadata(&mut bytes);
            let before = bytes;
            assert!(
                MappingPageRecord::decode(&bytes, &expected).is_err(),
                "offset={offset} value={value}"
            );
            assert_eq!(bytes, before);
        }
        for length in [0, 35, 3880, 4095] {
            assert!(MappingPageRecord::decode(&original[..length], &expected).is_err());
        }
        original[100] ^= 1;
        assert!(MappingPageRecord::decode(&original, &expected).is_err());
    }

    #[test]
    fn mapping_framing_survives_only_snapshot_body_damage() {
        let (record, expected) = fixture();
        let mut original = [0; BLOCK_SIZE];
        record.encode(expected.geometry, &mut original).unwrap();
        for offset in 0..BLOCK_SIZE {
            let mut damaged = original;
            damaged[offset] ^= 1;
            let framing =
                MappingPageRecord::decode_framing(&damaged, expected.geometry, expected.volume_id);
            if offset < ADDRESS_OFFSET || (HEADER_CHECKSUM_OFFSET..PADDING_OFFSET).contains(&offset)
            {
                assert!(framing.is_err(), "untrusted framing byte {offset}");
            } else {
                assert_eq!(framing.unwrap(), record.header, "body byte {offset}");
            }
            assert!(
                MappingPageRecord::decode(&damaged, &expected).is_err(),
                "damaged snapshot byte {offset}"
            );
        }
    }

    #[test]
    fn mapping_framing_checks_fields_after_header_checksum() {
        let (record, expected) = fixture();
        let mut original = [0; BLOCK_SIZE];
        record.encode(expected.geometry, &mut original).unwrap();
        for (offset, value) in [
            (0, 0),
            (4, 2),
            (5, 1),
            (6, 1),
            (8, 18),
            (16, 0),
            (24, 7),
            (28, 10),
            (32, 3),
            (36, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            let checksum = xxh3_64(&bytes[..40]);
            bytes[3880..3888].copy_from_slice(&checksum.to_le_bytes());
            assert!(
                MappingPageRecord::decode_framing(&bytes, expected.geometry, expected.volume_id)
                    .is_err()
            );
        }
        for length in [0, 39, 3887, 4095] {
            assert!(
                MappingPageRecord::decode_framing(
                    &original[..length],
                    expected.geometry,
                    expected.volume_id
                )
                .is_err()
            );
        }
    }

    #[test]
    fn all_entries_and_empty_pages_round_trip() {
        let (mut record, mut expected) = fixture();
        record.page_id = 0;
        expected.page_id = 0;
        for index in 0..320 {
            record.entries.physical_addr[index] = 4 + (index % 5) as u32;
            record.entries.payload_checksum[index] = index as u64;
        }
        let mut bytes = [0; BLOCK_SIZE];
        record.encode(expected.geometry, &mut bytes).unwrap();
        assert_eq!(read_u32(&bytes, 1316), record.entries.physical_addr[319]);
        assert_eq!(read_u64(&bytes, 3872), 319);
        assert_eq!(
            MappingPageRecord::decode(&bytes, &expected).unwrap(),
            record
        );
        record.entries = MappingEntries::zeroed();
        record.encode(expected.geometry, &mut bytes).unwrap();
        assert_eq!(
            MappingPageRecord::decode(&bytes, &expected).unwrap(),
            record
        );
        record.page_id = 3;
        assert!(record.encode(expected.geometry, &mut bytes).is_err());
    }
}
