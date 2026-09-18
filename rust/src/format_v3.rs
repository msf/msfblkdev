//! Explicit version-3 codecs. Persistent bytes are validated before use.
#![forbid(unsafe_code)]

use crate::{BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS, invalid_data, read_u32, read_u64};
use std::io;
use xxhash_rust::xxh3::Xxh3;

pub(crate) mod buffered_pages;
pub(crate) mod checkpoint;
pub(crate) mod checkpoint_load;
pub(crate) mod checkpoint_publish;
pub(crate) mod data_record;
pub(crate) mod geometry;
pub(crate) mod index;
pub(crate) mod mapping_page;
pub(crate) mod recovery;
pub(crate) mod reservation;
pub(crate) mod storage;
pub(crate) mod tail;

// ADR-07 assigns version 3 and reserves the final u64 for the metadata checksum.
pub(crate) const VERSION: u8 = 3;
pub(crate) const CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
pub(crate) const RECORD_MAGIC: &[u8; 4] = b"VBLF";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecordKind {
    Data = 1,
    MappingPage = 2,
}

impl TryFrom<u8> for RecordKind {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Data),
            2 => Ok(Self::MappingPage),
            _ => Err(invalid_data("unknown version-3 record kind")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordHeader {
    pub(crate) kind: RecordKind,
    pub(crate) volume_id: u64,
    pub(crate) lsn: u64,
    pub(crate) previous_boundary: u32,
    pub(crate) boundary: u32,
}

impl RecordHeader {
    pub(crate) fn payload_count(self) -> io::Result<usize> {
        let distance = self.boundary.checked_sub(self.previous_boundary);
        let count = match (self.kind, distance) {
            (RecordKind::MappingPage, Some(1)) => 0,
            (RecordKind::Data, Some(distance)) if (2..=339).contains(&distance) => {
                usize::try_from(distance - 1).expect("bounded payload count")
            }
            _ => return Err(invalid_data("invalid version-3 record length")),
        };
        if self.lsn == 0 {
            return Err(invalid_data("record LSN must be nonzero"));
        }
        assert!(count <= MAXIMUM_PAYLOAD_BLOCKS, "format payload limit");
        Ok(count)
    }

    pub(crate) fn follows(self, previous_lsn: u64, previous_boundary: u32) -> bool {
        Some(self.lsn) == previous_lsn.checked_add(1) && self.previous_boundary == previous_boundary
    }

    pub(crate) fn validate_location(
        self,
        volume_id: u64,
        log_start: u32,
        backing_blocks: u64,
    ) -> io::Result<()> {
        if self.volume_id != volume_id
            || log_start == 0
            || self.previous_boundary < log_start - 1
            || u64::from(self.boundary) >= backing_blocks
        {
            return Err(invalid_data("record identity or physical bounds mismatch"));
        }
        self.payload_count()?;
        Ok(())
    }

    pub(crate) fn encode(self, bytes: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        self.payload_count()?;
        bytes.fill(0);
        bytes[..4].copy_from_slice(RECORD_MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.kind as u8;
        bytes[8..16].copy_from_slice(&self.volume_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.lsn.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.previous_boundary.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.boundary.to_le_bytes());
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes = validate_metadata(bytes, RECORD_MAGIC)?;
        Self::decode_fields(bytes)
    }

    fn decode_fields(bytes: &[u8; BLOCK_SIZE]) -> io::Result<Self> {
        if &bytes[..4] != RECORD_MAGIC || bytes[4] != VERSION || bytes[6..8] != [0, 0] {
            return Err(invalid_data("record magic, version, or padding mismatch"));
        }
        let header = Self {
            kind: RecordKind::try_from(bytes[5])?,
            volume_id: read_u64(bytes, 8),
            lsn: read_u64(bytes, 16),
            previous_boundary: read_u32(bytes, 24),
            boundary: read_u32(bytes, 28),
        };
        header.payload_count()?;
        Ok(header)
    }
}

pub(crate) fn validate_metadata<'a>(
    bytes: &'a [u8],
    magic: &[u8; 4],
) -> io::Result<&'a [u8; BLOCK_SIZE]> {
    let block: &[u8; BLOCK_SIZE] = bytes
        .try_into()
        .map_err(|_| invalid_data("metadata block length"))?;
    if &block[..4] != magic || block[4] != VERSION {
        return Err(invalid_data("metadata magic or format version mismatch"));
    }
    if read_u64(block, CHECKSUM_OFFSET) != metadata_checksum(block) {
        return Err(invalid_data("metadata checksum mismatch"));
    }
    Ok(block)
}

pub(crate) fn metadata_checksum(bytes: &[u8; BLOCK_SIZE]) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(&bytes[..CHECKSUM_OFFSET]);
    hasher.update(&[0; size_of::<u64>()]);
    hasher.digest()
}

pub(crate) fn seal_metadata(bytes: &mut [u8; BLOCK_SIZE]) {
    let checksum = metadata_checksum(bytes);
    bytes[CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: RecordKind, previous_boundary: u32) -> RecordHeader {
        RecordHeader {
            kind,
            volume_id: 0x0102_0304_0506_0708,
            lsn: 42,
            previous_boundary,
            boundary: previous_boundary + if kind == RecordKind::Data { 2 } else { 1 },
        }
    }

    #[test]
    fn shared_header_has_exact_bytes_and_checksum() {
        for kind in [RecordKind::Data, RecordKind::MappingPage] {
            let expected = header(kind, 100);
            let mut bytes = [0xff; BLOCK_SIZE];
            expected.encode(&mut bytes).unwrap();
            seal_metadata(&mut bytes);
            assert_eq!(&bytes[..8], &[b'V', b'B', b'L', b'F', 3, kind as u8, 0, 0]);
            assert_eq!(&bytes[8..16], &[8, 7, 6, 5, 4, 3, 2, 1]);
            assert_eq!(&bytes[16..24], &42_u64.to_le_bytes());
            assert_eq!(&bytes[24..28], &100_u32.to_le_bytes());
            assert_eq!(&bytes[28..32], &expected.boundary.to_le_bytes());
            assert!(bytes[32..CHECKSUM_OFFSET].iter().all(|byte| *byte == 0));
            let mut independently_hashed = bytes;
            independently_hashed[4088..].fill(0);
            assert_eq!(
                read_u64(&bytes, 4088),
                xxhash_rust::xxh3::xxh3_64(&independently_hashed)
            );
            assert_eq!(RecordHeader::decode(&bytes).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_versions_kinds_padding_truncation_and_corruption_without_mutation() {
        let mut original = [0; BLOCK_SIZE];
        header(RecordKind::Data, 100).encode(&mut original).unwrap();
        seal_metadata(&mut original);
        for (offset, value) in [
            (0, 0),
            (4, 1),
            (4, 2),
            (4, 4),
            (5, 0),
            (5, 3),
            (6, 1),
            (7, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            seal_metadata(&mut bytes);
            let before = bytes;
            assert!(RecordHeader::decode(&bytes).is_err());
            assert_eq!(bytes, before);
        }
        for length in [0, 31, 32, 4095] {
            assert!(RecordHeader::decode(&original[..length]).is_err());
        }
        assert!(RecordHeader::decode(&[0; BLOCK_SIZE + 1]).is_err());
        original[4095] ^= 1;
        assert!(RecordHeader::decode(&original).is_err());
    }

    #[test]
    fn validates_record_lengths_identity_and_chain_in_both_orders() {
        for first_kind in [RecordKind::Data, RecordKind::MappingPage] {
            let first = header(first_kind, 100);
            let mut second = header(
                if first_kind == RecordKind::Data {
                    RecordKind::MappingPage
                } else {
                    RecordKind::Data
                },
                first.boundary,
            );
            second.lsn = first.lsn + 1;
            assert!(second.follows(first.lsn, first.boundary));
            assert!(!second.follows(first.lsn + 1, first.boundary));
            assert!(!second.follows(first.lsn, first.boundary + 1));
            assert!(!second.follows(u64::MAX, first.boundary));
            assert!(second.validate_location(first.volume_id, 4, 200).is_ok());
            assert!(second.validate_location(0, 4, 200).is_err());
            assert!(second.validate_location(first.volume_id, 0, 200).is_err());
            assert!(second.validate_location(first.volume_id, 150, 200).is_err());
            assert!(
                second
                    .validate_location(first.volume_id, 4, u64::from(second.boundary))
                    .is_err()
            );
        }
        let mut data = header(RecordKind::Data, 100);
        for count in 1..=338 {
            data.boundary = 101 + count;
            assert_eq!(data.payload_count().unwrap(), count as usize);
        }
        for boundary in [0, 99, 100, 101, 440, u32::MAX] {
            data.boundary = boundary;
            assert!(data.payload_count().is_err());
        }
        let mut page = header(RecordKind::MappingPage, 100);
        page.boundary = 102;
        assert!(page.payload_count().is_err());
        page.boundary = 101;
        page.lsn = 0;
        assert!(page.payload_count().is_err());
    }
}
