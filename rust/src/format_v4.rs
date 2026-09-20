//! ADR-08 format-4 codecs. Persistent bytes are validated before use; nothing here performs I/O.
#![forbid(unsafe_code)]
// The engine still runs format 3; this attribute must go when the volume moves to format 4.
#![expect(dead_code, reason = "format 4 is not wired into the engine yet")]

use crate::{BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS, invalid_data, read_u32, read_u64};
use std::io;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

pub(crate) mod checkpoint_load;
pub(crate) mod checkpoint_publish;
pub(crate) mod data_record;
pub(crate) mod descriptor;
pub(crate) mod geometry;
pub(crate) mod heads;
pub(crate) mod index_page;
pub(crate) mod log;
pub(crate) mod mapping_index;
pub(crate) mod mapping_page;
pub(crate) mod recovery;
pub(crate) mod segment_table;
pub(crate) mod segments;
pub(crate) mod small_records;
pub(crate) mod storage;
pub(crate) mod tail;

pub(crate) use geometry::Geometry;

pub(crate) const VERSION: u8 = 4;
pub(crate) const RECORD_MAGIC: &[u8; 4] = b"VBLF";
pub(crate) const DESCRIPTOR_MAGIC: &[u8; 4] = b"VBLC";
/// Every 4 KiB metadata block ends with its XXH3-64 checksum.
pub(crate) const CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
/// One-block metadata records carry an independent checksum over bytes 0..40 so that recovery can
/// traverse a record whose body is damaged.
pub(crate) const HEADER_CHECKSUM_OFFSET: usize = 4072;
pub(crate) const HEADER_CHECKSUM_COVERAGE: usize = 40;
pub(crate) const TRAILER_PADDING: std::ops::Range<usize> = 4080..CHECKSUM_OFFSET;
/// Bytes 32..4072 of a one-block metadata record hold its kind-specific body.
pub(crate) const BODY_END: usize = HEADER_CHECKSUM_OFFSET;
/// Blocks 0 and 1 hold the two checkpoint descriptors; every segment starts at block 2 or later.
pub(crate) const FIRST_SEGMENT_BLOCK: u32 = 2;
/// `previous_boundary` of the first record on a head: no earlier record exists on that chain.
pub(crate) const NO_PREVIOUS_BOUNDARY: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum RecordKind {
    Data = 1,
    MappingPage = 2,
    SegmentTablePage = 3,
    SegmentLink = 4,
    BackingGrowth = 5,
    // 6 is reserved for a future compressed record (ADR-09); format-4 readers reject it.
    LbaDiscard = 7,
    MappingIndexPage = 8,
}

impl TryFrom<u8> for RecordKind {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Data),
            2 => Ok(Self::MappingPage),
            3 => Ok(Self::SegmentTablePage),
            4 => Ok(Self::SegmentLink),
            5 => Ok(Self::BackingGrowth),
            7 => Ok(Self::LbaDiscard),
            8 => Ok(Self::MappingIndexPage),
            _ => Err(invalid_data("unknown or reserved format-4 record kind")),
        }
    }
}

impl RecordKind {
    pub(crate) const fn is_data(self) -> bool {
        matches!(self, Self::Data)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum HeadId {
    User = 0,
    Relocation = 1,
}

impl HeadId {
    pub(crate) const ALL: [Self; 2] = [Self::User, Self::Relocation];

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn from_index(index: usize) -> Self {
        match index {
            0 => Self::User,
            1 => Self::Relocation,
            _ => unreachable!("two fixed heads"),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Relocation => "relocation",
        }
    }
}

impl TryFrom<u8> for HeadId {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::User),
            1 => Ok(Self::Relocation),
            _ => Err(invalid_data("unknown head identifier")),
        }
    }
}

/// The first 32 bytes of every record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordHeader {
    pub(crate) kind: RecordKind,
    pub(crate) volume_id: u64,
    pub(crate) lsn: u64,
    pub(crate) previous_boundary: u32,
    pub(crate) boundary: u32,
}

impl RecordHeader {
    /// The first physical block of this record. A record follows its predecessor inside one segment;
    /// after a `SegmentLink`, or for the first record on a head, it starts its segment.
    pub(crate) fn start(self, geometry: Geometry) -> io::Result<u32> {
        let segment = geometry.segment_of(self.boundary)?;
        let segment_start = geometry.segment_start(segment)?;
        if self.previous_boundary == NO_PREVIOUS_BOUNDARY {
            return Ok(segment_start);
        }
        if self.previous_boundary < FIRST_SEGMENT_BLOCK {
            return Err(invalid_data("record chains from a descriptor block"));
        }
        if geometry.segment_of(self.previous_boundary)? != segment {
            return Ok(segment_start);
        }
        if self.previous_boundary >= self.boundary {
            return Err(invalid_data(
                "record boundary does not follow its predecessor",
            ));
        }
        Ok(self.previous_boundary + 1)
    }

    /// Payload blocks between `start` and `boundary`; zero for every one-block kind.
    pub(crate) fn payload_count(self, geometry: Geometry) -> io::Result<usize> {
        if self.lsn == 0 {
            return Err(invalid_data("record LSN must be nonzero"));
        }
        let count = usize::try_from(self.boundary - self.start(geometry)?)
            .expect("block distance fits usize");
        match self.kind {
            RecordKind::Data if (1..=MAXIMUM_PAYLOAD_BLOCKS).contains(&count) => Ok(count),
            RecordKind::Data => Err(invalid_data("invalid data-record payload count")),
            _ if count == 0 => Ok(0),
            _ => Err(invalid_data("one-block record spans several blocks")),
        }
    }

    pub(crate) fn validate(self, geometry: Geometry, volume_id: u64) -> io::Result<()> {
        if self.volume_id != volume_id {
            return Err(invalid_data("record volume mismatch"));
        }
        self.payload_count(geometry)?;
        Ok(())
    }

    /// Whether this record is the next one on a head whose last record ended at `previous`
    /// with LSN `previous_lsn`. LSNs are global, so a head chain can skip values.
    pub(crate) fn chains_from(self, previous_lsn: u64, previous_boundary: u32) -> bool {
        self.lsn > previous_lsn && self.previous_boundary == previous_boundary
    }

    pub(crate) fn encode(self, bytes: &mut [u8; BLOCK_SIZE]) {
        bytes.fill(0);
        bytes[..4].copy_from_slice(RECORD_MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.kind as u8;
        bytes[8..16].copy_from_slice(&self.volume_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.lsn.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.previous_boundary.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.boundary.to_le_bytes());
    }

    pub(crate) fn decode_fields(bytes: &[u8; BLOCK_SIZE]) -> io::Result<Self> {
        if &bytes[..4] != RECORD_MAGIC || bytes[4] != VERSION || bytes[6..8] != [0, 0] {
            return Err(invalid_data("record magic, version, or padding mismatch"));
        }
        Ok(Self {
            kind: RecordKind::try_from(bytes[5])?,
            volume_id: read_u64(bytes, 8),
            lsn: read_u64(bytes, 16),
            previous_boundary: read_u32(bytes, 24),
            boundary: read_u32(bytes, 28),
        })
    }

    /// Decodes a fully checksummed record header (data footers and complete metadata records).
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        Self::decode_fields(validate_block_checksum(bytes, RECORD_MAGIC)?)
    }
}

/// Whether a block starts like one of this volume's records. Used to classify damaged tails.
pub(crate) fn looks_like_record(bytes: &[u8; BLOCK_SIZE], volume_id: u64) -> bool {
    &bytes[..4] == RECORD_MAGIC && read_u64(bytes, 8) == volume_id
}

/// Whether a block that looks like a record has a version or kind this engine cannot interpret.
pub(crate) fn unsupported_record(bytes: &[u8; BLOCK_SIZE]) -> bool {
    bytes[4] != VERSION || RecordKind::try_from(bytes[5]).is_err()
}

pub(crate) fn validate_block_checksum<'a>(
    bytes: &'a [u8],
    magic: &[u8; 4],
) -> io::Result<&'a [u8; BLOCK_SIZE]> {
    let block: &[u8; BLOCK_SIZE] = bytes
        .try_into()
        .map_err(|_| invalid_data("metadata block length"))?;
    if &block[..4] != magic || block[4] != VERSION {
        return Err(invalid_data("metadata magic or format version mismatch"));
    }
    if read_u64(block, CHECKSUM_OFFSET) != block_checksum(block) {
        return Err(invalid_data("metadata checksum mismatch"));
    }
    Ok(block)
}

/// XXH3-64 over the complete block with its final checksum field zeroed (inherited from format 3).
pub(crate) fn block_checksum(bytes: &[u8; BLOCK_SIZE]) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(&bytes[..CHECKSUM_OFFSET]);
    hasher.update(&[0; size_of::<u64>()]);
    hasher.digest()
}

pub(crate) fn seal_block(bytes: &mut [u8; BLOCK_SIZE]) {
    let checksum = block_checksum(bytes);
    bytes[CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
}

/// Seals a one-block metadata record: independent header checksum, zero padding, block checksum.
pub(crate) fn seal_metadata_record(bytes: &mut [u8; BLOCK_SIZE]) {
    let header_checksum = xxh3_64(&bytes[..HEADER_CHECKSUM_COVERAGE]);
    bytes[HEADER_CHECKSUM_OFFSET..TRAILER_PADDING.start]
        .copy_from_slice(&header_checksum.to_le_bytes());
    bytes[TRAILER_PADDING].fill(0);
    seal_block(bytes);
}

/// Validates only the traversal fields of a one-block metadata record. The body may be damaged.
pub(crate) fn decode_metadata_framing(
    bytes: &[u8],
    geometry: Geometry,
    volume_id: u64,
) -> io::Result<RecordHeader> {
    let bytes: &[u8; BLOCK_SIZE] = bytes
        .try_into()
        .map_err(|_| invalid_data("metadata record length"))?;
    if read_u64(bytes, HEADER_CHECKSUM_OFFSET) != xxh3_64(&bytes[..HEADER_CHECKSUM_COVERAGE]) {
        return Err(invalid_data("metadata header checksum mismatch"));
    }
    let header = RecordHeader::decode_fields(bytes)?;
    if header.kind.is_data() {
        return Err(invalid_data(
            "data records have no independent header checksum",
        ));
    }
    header.validate(geometry, volume_id)?;
    Ok(header)
}

/// Validates a complete one-block metadata record and returns its header.
pub(crate) fn decode_metadata_record(
    bytes: &[u8],
    geometry: Geometry,
    volume_id: u64,
) -> io::Result<(RecordHeader, &[u8; BLOCK_SIZE])> {
    let header = decode_metadata_framing(bytes, geometry, volume_id)?;
    let block = validate_block_checksum(bytes, RECORD_MAGIC)?;
    if block[TRAILER_PADDING].iter().any(|byte| *byte != 0) {
        return Err(invalid_data("metadata trailer padding must be zero"));
    }
    Ok((header, block))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Reseals a mutated metadata record. A mutation inside the trailer padding must survive,
    /// so only the block checksum is recomputed for it.
    pub(crate) fn reseal_metadata(bytes: &mut [u8; BLOCK_SIZE], offset: usize) {
        if TRAILER_PADDING.contains(&offset) {
            seal_block(bytes);
        } else {
            seal_metadata_record(bytes);
        }
    }
}

pub(crate) fn require_zero(bytes: &[u8], message: &'static str) -> io::Result<()> {
    if bytes.iter().any(|byte| *byte != 0) {
        return Err(invalid_data(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry::new(4096, 2 + 8 * 64, 64).unwrap()
    }

    fn header(kind: RecordKind, previous_boundary: u32, boundary: u32) -> RecordHeader {
        RecordHeader {
            kind,
            volume_id: 0x0102_0304_0506_0708,
            lsn: 42,
            previous_boundary,
            boundary,
        }
    }

    #[test]
    fn header_has_exact_bytes_and_every_kind_round_trips() {
        for kind in [
            RecordKind::Data,
            RecordKind::MappingPage,
            RecordKind::SegmentTablePage,
            RecordKind::SegmentLink,
            RecordKind::BackingGrowth,
            RecordKind::LbaDiscard,
            RecordKind::MappingIndexPage,
        ] {
            let expected = header(kind, 100, 101 + usize::from(kind.is_data()) as u32);
            let mut bytes = [0xff; BLOCK_SIZE];
            expected.encode(&mut bytes);
            seal_block(&mut bytes);
            assert_eq!(&bytes[..8], &[b'V', b'B', b'L', b'F', 4, kind as u8, 0, 0]);
            assert_eq!(&bytes[8..16], &[8, 7, 6, 5, 4, 3, 2, 1]);
            assert_eq!(&bytes[16..24], &42_u64.to_le_bytes());
            assert_eq!(&bytes[24..28], &100_u32.to_le_bytes());
            assert_eq!(&bytes[28..32], &expected.boundary.to_le_bytes());
            assert!(bytes[32..CHECKSUM_OFFSET].iter().all(|byte| *byte == 0));
            let mut independent = bytes;
            independent[CHECKSUM_OFFSET..].fill(0);
            assert_eq!(read_u64(&bytes, CHECKSUM_OFFSET), xxh3_64(&independent));
            assert_eq!(RecordHeader::decode(&bytes).unwrap(), expected);
            assert_eq!(RecordKind::try_from(kind as u8).unwrap(), kind);
        }
        for value in [0, 6, 9, 255] {
            assert!(RecordKind::try_from(value).is_err(), "kind {value}");
        }
        assert_eq!(HeadId::try_from(0).unwrap(), HeadId::User);
        assert_eq!(HeadId::try_from(1).unwrap(), HeadId::Relocation);
        assert!(HeadId::try_from(2).is_err());
    }

    #[test]
    fn rejects_versions_kinds_padding_truncation_and_corruption() {
        let mut original = [0; BLOCK_SIZE];
        header(RecordKind::Data, 100, 102).encode(&mut original);
        seal_block(&mut original);
        for (offset, value) in [(0, 0), (4, 3), (4, 5), (5, 0), (5, 6), (6, 1), (7, 1)] {
            let mut bytes = original;
            bytes[offset] = value;
            seal_block(&mut bytes);
            assert!(RecordHeader::decode(&bytes).is_err(), "offset {offset}");
        }
        for length in [0, 31, 32, 4095] {
            assert!(RecordHeader::decode(&original[..length]).is_err());
        }
        original[4095] ^= 1;
        assert!(RecordHeader::decode(&original).is_err());
    }

    #[test]
    fn frame_rule_follows_predecessor_inside_a_segment_and_restarts_after_a_link() {
        let geometry = geometry();
        // Segment 1 spans blocks 66..130.
        let first = header(RecordKind::Data, NO_PREVIOUS_BOUNDARY, 70);
        assert_eq!(first.start(geometry).unwrap(), 66);
        assert_eq!(first.payload_count(geometry).unwrap(), 4);
        let inside = header(RecordKind::Data, 70, 75);
        assert_eq!(inside.start(geometry).unwrap(), 71);
        assert_eq!(inside.payload_count(geometry).unwrap(), 4);
        // The predecessor is a link at the end of segment 0 (block 65).
        let after_link = header(RecordKind::Data, 65, 68);
        assert_eq!(after_link.start(geometry).unwrap(), 66);
        assert_eq!(after_link.payload_count(geometry).unwrap(), 2);
        let metadata = header(RecordKind::SegmentLink, 75, 76);
        assert_eq!(metadata.payload_count(geometry).unwrap(), 0);
        let first_metadata = header(RecordKind::MappingPage, 65, 66);
        assert_eq!(first_metadata.payload_count(geometry).unwrap(), 0);
        let mut full = header(RecordKind::Data, NO_PREVIOUS_BOUNDARY, 66 + 338);
        let big = Geometry::new(4096, 2 + 4 * 1024, 1024).unwrap();
        full.boundary = 2 + 1024 + 338;
        assert_eq!(full.payload_count(big).unwrap(), 338);
        full.boundary += 1;
        assert!(full.payload_count(big).is_err());
    }

    #[test]
    fn frame_rule_rejects_backward_chains_descriptor_blocks_and_multi_block_metadata() {
        let geometry = geometry();
        assert!(
            header(RecordKind::Data, 70, 70)
                .payload_count(geometry)
                .is_err()
        );
        assert!(
            header(RecordKind::Data, 71, 70)
                .payload_count(geometry)
                .is_err()
        );
        assert!(
            header(RecordKind::Data, 1, 70)
                .payload_count(geometry)
                .is_err()
        );
        assert!(
            header(RecordKind::Data, 70, 71)
                .payload_count(geometry)
                .is_err()
        );
        assert!(
            header(RecordKind::SegmentLink, 70, 72)
                .payload_count(geometry)
                .is_err()
        );
        assert!(
            header(RecordKind::MappingPage, 65, 67)
                .payload_count(geometry)
                .is_err()
        );
        let mut zero_lsn = header(RecordKind::Data, 70, 72);
        zero_lsn.lsn = 0;
        assert!(zero_lsn.payload_count(geometry).is_err());
        // Boundaries outside every whole segment.
        for boundary in [0, 1, 2 + 8 * 64, u32::MAX] {
            assert!(
                header(RecordKind::Data, NO_PREVIOUS_BOUNDARY, boundary)
                    .start(geometry)
                    .is_err(),
                "boundary {boundary}"
            );
        }
        assert!(
            header(RecordKind::Data, 70, 72)
                .validate(geometry, 1)
                .is_err()
        );
        assert!(
            header(RecordKind::Data, 70, 72)
                .validate(geometry, 0x0102_0304_0506_0708)
                .is_ok()
        );
    }

    #[test]
    fn metadata_trailer_separates_framing_from_body_integrity() {
        let geometry = geometry();
        let expected = header(RecordKind::SegmentLink, 70, 71);
        let mut bytes = [0; BLOCK_SIZE];
        expected.encode(&mut bytes);
        bytes[32..36].copy_from_slice(&3_u32.to_le_bytes());
        seal_metadata_record(&mut bytes);
        assert_eq!(read_u64(&bytes, 4072), xxh3_64(&bytes[..40]));
        assert!(bytes[4080..4088].iter().all(|byte| *byte == 0));
        let (decoded, _) = decode_metadata_record(&bytes, geometry, expected.volume_id).unwrap();
        assert_eq!(decoded, expected);
        for offset in 0..BLOCK_SIZE {
            let mut damaged = bytes;
            damaged[offset] ^= 1;
            let framing = decode_metadata_framing(&damaged, geometry, expected.volume_id);
            if offset < 40 || (4072..4080).contains(&offset) {
                assert!(framing.is_err(), "framing byte {offset}");
            } else {
                assert_eq!(framing.unwrap(), expected, "body byte {offset}");
            }
            assert!(decode_metadata_record(&damaged, geometry, expected.volume_id).is_err());
        }
        let mut data = [0; BLOCK_SIZE];
        header(RecordKind::Data, 70, 72).encode(&mut data);
        seal_metadata_record(&mut data);
        assert!(decode_metadata_framing(&data, geometry, expected.volume_id).is_err());
        assert!(looks_like_record(&bytes, expected.volume_id));
        assert!(!looks_like_record(&bytes, 1));
        assert!(!unsupported_record(&bytes));
        bytes[5] = 6;
        assert!(unsupported_record(&bytes));
        bytes[5] = 4;
        bytes[4] = 3;
        assert!(unsupported_record(&bytes));
    }
}
