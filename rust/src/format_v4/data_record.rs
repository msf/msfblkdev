use super::{Geometry, RecordHeader, RecordKind, seal_block};
use crate::{BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS, invalid_data, read_u32, read_u64};
use std::io;

// ADR-08 retains ADR-01's fixed data-footer arrays.
const LBA_OFFSET: usize = 32;
const PAYLOAD_CHECKSUM_OFFSET: usize = 1384;

/// A raw data record: 1 to 338 payload blocks followed by this footer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DataRecord {
    pub(crate) header: RecordHeader,
    pub(crate) lba_ids: [u32; MAXIMUM_PAYLOAD_BLOCKS],
    pub(crate) checksums: [u64; MAXIMUM_PAYLOAD_BLOCKS],
}

impl DataRecord {
    pub(crate) fn empty(header: RecordHeader) -> Self {
        Self {
            header,
            lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
            checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
        }
    }

    /// The physical block of payload `index`.
    pub(crate) fn payload_block(&self, geometry: Geometry, index: usize) -> io::Result<u32> {
        let count = self.header.payload_count(geometry)?;
        if index >= count {
            return Err(invalid_data("payload index beyond the record"));
        }
        Ok(self.header.start(geometry)? + index as u32)
    }

    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::Data {
            return Err(invalid_data("expected data record"));
        }
        let count = self.header.payload_count(geometry)?;
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
        self.header.encode(bytes);
        for index in 0..MAXIMUM_PAYLOAD_BLOCKS {
            let lba = LBA_OFFSET + index * 4;
            let checksum = PAYLOAD_CHECKSUM_OFFSET + index * 8;
            bytes[lba..lba + 4].copy_from_slice(&self.lba_ids[index].to_le_bytes());
            bytes[checksum..checksum + 8].copy_from_slice(&self.checksums[index].to_le_bytes());
        }
        seal_block(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], geometry: Geometry, volume_id: u64) -> io::Result<Self> {
        let header = RecordHeader::decode(bytes)?;
        header.validate(geometry, volume_id)?;
        let mut record = Self::empty(header);
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
    use super::*;
    use crate::format_v4::NO_PREVIOUS_BOUNDARY;

    fn geometry() -> Geometry {
        Geometry::new(512, 2 + 4 * 1024, 1024).unwrap()
    }

    fn record(count: usize, previous_boundary: u32) -> DataRecord {
        let boundary = if previous_boundary == NO_PREVIOUS_BOUNDARY {
            2 + 1024 + count as u32
        } else {
            previous_boundary + 1 + count as u32
        };
        let mut record = DataRecord::empty(RecordHeader {
            kind: RecordKind::Data,
            volume_id: 7,
            lsn: 1,
            previous_boundary,
            boundary,
        });
        for index in 0..count {
            record.lba_ids[index] = index as u32;
            record.checksums[index] = 0x0102_0304_0506_0708 + index as u64;
        }
        record
    }

    #[test]
    fn every_payload_count_round_trips_from_both_segment_positions() {
        let geometry = geometry();
        for count in 1..=MAXIMUM_PAYLOAD_BLOCKS {
            for previous in [NO_PREVIOUS_BOUNDARY, 2 + 1024 + 10] {
                let record = record(count, previous);
                let mut bytes = [0xff; BLOCK_SIZE];
                record.encode(geometry, &mut bytes).unwrap();
                assert_eq!(bytes[5], 1);
                assert_eq!(read_u32(&bytes, 32 + (count - 1) * 4), count as u32 - 1);
                assert!(bytes[32 + count * 4..1384].iter().all(|byte| *byte == 0));
                assert!(bytes[1384 + count * 8..4088].iter().all(|byte| *byte == 0));
                let decoded = DataRecord::decode(&bytes, geometry, 7).unwrap();
                assert_eq!(decoded, record);
                let start = if previous == NO_PREVIOUS_BOUNDARY {
                    2 + 1024
                } else {
                    previous + 1
                };
                assert_eq!(decoded.payload_block(geometry, 0).unwrap(), start);
                assert_eq!(
                    decoded.payload_block(geometry, count - 1).unwrap(),
                    start + count as u32 - 1
                );
                assert!(decoded.payload_block(geometry, count).is_err());
            }
        }
    }

    #[test]
    fn data_record_after_a_link_starts_its_segment() {
        let geometry = geometry();
        // The link closed segment 0 at block 1025; the record starts segment 2.
        let record = DataRecord {
            header: RecordHeader {
                kind: RecordKind::Data,
                volume_id: 7,
                lsn: 5,
                previous_boundary: 2 + 1023,
                boundary: 2 + 2 * 1024 + 2,
            },
            ..record(2, NO_PREVIOUS_BOUNDARY)
        };
        let mut bytes = [0; BLOCK_SIZE];
        record.encode(geometry, &mut bytes).unwrap();
        let decoded = DataRecord::decode(&bytes, geometry, 7).unwrap();
        assert_eq!(decoded.payload_block(geometry, 0).unwrap(), 2 + 2 * 1024);
        assert_eq!(
            decoded.payload_block(geometry, 1).unwrap(),
            2 + 2 * 1024 + 1
        );
    }

    #[test]
    fn rejects_duplicate_out_of_range_unused_and_misframed_footers() {
        let geometry = geometry();
        let mut original = [0; BLOCK_SIZE];
        record(2, NO_PREVIOUS_BOUNDARY)
            .encode(geometry, &mut original)
            .unwrap();
        for (offset, value) in [
            (36, 0_u32),
            (32, 512),
            (40, 1),
            (1400, 1),
            (24, 2 + 1024 + 2),
            (28, 2 + 1024),
            (28, 2 + 1024 + 339),
            (28, 2 + 4 * 1024),
        ] {
            let mut bytes = original;
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            seal_block(&mut bytes);
            assert!(DataRecord::decode(&bytes, geometry, 7).is_err(), "{offset}");
        }
        assert!(DataRecord::decode(&original, geometry, 8).is_err());
        for length in [0, 32, 1384, 4095] {
            assert!(DataRecord::decode(&original[..length], geometry, 7).is_err());
        }
        original[4088] ^= 1;
        assert!(DataRecord::decode(&original, geometry, 7).is_err());
    }
}
