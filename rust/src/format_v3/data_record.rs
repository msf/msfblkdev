use super::{RecordHeader, RecordKind, geometry::Geometry, seal_metadata};
use crate::{BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS, invalid_data, read_u32, read_u64};
use std::io;

// ADR-07 retains ADR-01's fixed data-footer arrays.
const LBA_OFFSET: usize = 32;
const PAYLOAD_CHECKSUM_OFFSET: usize = 1384;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DataRecord {
    pub(crate) header: RecordHeader,
    pub(crate) lba_ids: [u32; MAXIMUM_PAYLOAD_BLOCKS],
    pub(crate) checksums: [u64; MAXIMUM_PAYLOAD_BLOCKS],
}

impl DataRecord {
    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::Data {
            return Err(invalid_data("expected data record"));
        }
        self.header.validate_location(
            self.header.volume_id,
            geometry.log_start,
            geometry.backing_blocks,
        )?;
        let count = self.header.payload_count()?;
        for (index, lba) in self.lba_ids.iter().take(count).enumerate() {
            if *lba >= geometry.volume_blocks || self.lba_ids[..index].contains(lba) {
                return Err(invalid_data("out-of-range or duplicate record LBA"));
            }
        }
        if self.lba_ids[count..].iter().any(|lba| *lba != 0)
            || self.checksums[count..]
                .iter()
                .any(|checksum| *checksum != 0)
        {
            return Err(invalid_data("nonzero unused data-footer entry"));
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
        for index in 0..MAXIMUM_PAYLOAD_BLOCKS {
            let lba = LBA_OFFSET + index * 4;
            let checksum = PAYLOAD_CHECKSUM_OFFSET + index * 8;
            bytes[lba..lba + 4].copy_from_slice(&self.lba_ids[index].to_le_bytes());
            bytes[checksum..checksum + 8].copy_from_slice(&self.checksums[index].to_le_bytes());
        }
        seal_metadata(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], geometry: Geometry, volume_id: u64) -> io::Result<Self> {
        let header = RecordHeader::decode(bytes)?;
        if header.volume_id != volume_id {
            return Err(invalid_data("data-record volume mismatch"));
        }
        let mut record = Self {
            header,
            lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
            checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
        };
        for index in 0..MAXIMUM_PAYLOAD_BLOCKS {
            record.lba_ids[index] = read_u32(bytes, LBA_OFFSET + index * 4);
            record.checksums[index] = read_u64(bytes, PAYLOAD_CHECKSUM_OFFSET + index * 8);
        }
        record.validate(geometry)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::super::mapping_page::{MappingPageRecord, PageExpectation};
    use super::*;
    use crate::mapping_cache::MappingEntries;

    fn record(count: usize) -> DataRecord {
        let mut record = DataRecord {
            header: RecordHeader {
                kind: RecordKind::Data,
                volume_id: 7,
                lsn: 1,
                previous_boundary: 3,
                boundary: 4 + count as u32,
            },
            lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
            checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
        };
        for index in 0..count {
            record.lba_ids[index] = index as u32;
            record.checksums[index] = 0x0102_0304_0506_0708 + index as u64;
        }
        record
    }

    #[test]
    fn every_payload_count_round_trips_with_exact_footer_arrays() {
        let geometry = Geometry::new(512, 1000).unwrap();
        for count in 1..=MAXIMUM_PAYLOAD_BLOCKS {
            let record = record(count);
            let mut bytes = [0xff; BLOCK_SIZE];
            record.encode(geometry, &mut bytes).unwrap();
            assert_eq!(bytes[5], 1);
            for index in 0..count {
                assert_eq!(read_u32(&bytes, 32 + index * 4), index as u32);
                assert_eq!(read_u64(&bytes, 1384 + index * 8), record.checksums[index]);
            }
            assert!(bytes[32 + count * 4..1384].iter().all(|byte| *byte == 0));
            assert!(bytes[1384 + count * 8..4088].iter().all(|byte| *byte == 0));
            let mut independent = bytes;
            independent[4088..].fill(0);
            assert_eq!(
                read_u64(&bytes, 4088),
                xxhash_rust::xxh3::xxh3_64(&independent)
            );
            assert_eq!(DataRecord::decode(&bytes, geometry, 7).unwrap(), record);
        }
    }

    #[test]
    fn rejects_duplicate_out_of_range_and_unused_entries() {
        let geometry = Geometry::new(512, 1000).unwrap();
        let mut original = [0; BLOCK_SIZE];
        record(2).encode(geometry, &mut original).unwrap();
        for (offset, value) in [
            (36, 0_u32),
            (32, 512),
            (40, 1),
            (1400, 1),
            (24, 2),
            (28, 1000),
        ] {
            let mut bytes = original;
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            seal_metadata(&mut bytes);
            assert!(DataRecord::decode(&bytes, geometry, 7).is_err());
        }
        assert!(DataRecord::decode(&original, geometry, 8).is_err());
        for length in [0, 32, 1384, 4095] {
            assert!(DataRecord::decode(&original[..length], geometry, 7).is_err());
        }
        original[4088] ^= 1;
        assert!(DataRecord::decode(&original, geometry, 7).is_err());
    }

    #[test]
    fn complete_data_and_mapping_records_chain_in_both_orders() {
        let geometry = Geometry::new(512, 1000).unwrap();
        let mut data = record(2);
        let mut page = MappingPageRecord {
            header: RecordHeader {
                kind: RecordKind::MappingPage,
                volume_id: 7,
                lsn: 2,
                previous_boundary: 6,
                boundary: 7,
            },
            page_id: 0,
            entries: MappingEntries::zeroed(),
        };
        page.entries.physical_addr[0] = 4;
        page.entries.payload_checksum[0] = data.checksums[0];
        let mut bytes = [0; BLOCK_SIZE];
        data.encode(geometry, &mut bytes).unwrap();
        let decoded_data = DataRecord::decode(&bytes, geometry, 7).unwrap();
        page.encode(geometry, &mut bytes).unwrap();
        let decoded_page = MappingPageRecord::decode(
            &bytes,
            &PageExpectation {
                geometry,
                volume_id: 7,
                page_id: 0,
                boundary: 7,
                maximum_lsn: 2,
            },
        )
        .unwrap();
        assert!(
            decoded_page
                .header
                .follows(decoded_data.header.lsn, decoded_data.header.boundary)
        );
        data.header.previous_boundary = 7;
        data.header.boundary = 10;
        data.header.lsn = 3;
        data.encode(geometry, &mut bytes).unwrap();
        let decoded_data = DataRecord::decode(&bytes, geometry, 7).unwrap();
        assert!(
            decoded_data
                .header
                .follows(decoded_page.header.lsn, decoded_page.header.boundary)
        );
        assert!(
            MappingPageRecord::decode(
                &bytes,
                &PageExpectation {
                    geometry,
                    volume_id: 7,
                    page_id: 0,
                    boundary: 10,
                    maximum_lsn: 3,
                }
            )
            .is_err()
        );
    }
}
