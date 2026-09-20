//! One-block records with small bodies: `SegmentLink`, `BackingGrowth` and `LbaDiscard`.
use super::{
    BODY_END, Geometry, HeadId, RecordHeader, RecordKind, decode_metadata_record, require_zero,
    seal_metadata_record,
};
use crate::{BLOCK_SIZE, MAX_BACKING_BLOCKS, invalid_data, read_u32, read_u64};
use std::io;

/// Ranges per `LbaDiscard`: (4072 - 40) / (4 + 4).
pub(crate) const DISCARD_RANGES: usize = 504;
const DISCARD_START_OFFSET: usize = 40;
const DISCARD_COUNT_OFFSET: usize = DISCARD_START_OFFSET + DISCARD_RANGES * 4;

/// Closes the segment holding it and names the next segment of the same head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentLink {
    pub(crate) header: RecordHeader,
    pub(crate) next_segment: u32,
    pub(crate) head_id: HeadId,
}

impl SegmentLink {
    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::SegmentLink {
            return Err(invalid_data("expected segment link"));
        }
        self.header.validate(geometry, self.header.volume_id)?;
        if self.next_segment >= geometry.segment_count
            || geometry.segment_of(self.header.boundary)? == self.next_segment
        {
            return Err(invalid_data("segment link names an invalid next segment"));
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
        bytes[32..36].copy_from_slice(&self.next_segment.to_le_bytes());
        bytes[36] = self.head_id as u8;
        seal_metadata_record(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], geometry: Geometry, volume_id: u64) -> io::Result<Self> {
        let (header, bytes) = decode_metadata_record(bytes, geometry, volume_id)?;
        if header.kind != RecordKind::SegmentLink {
            return Err(invalid_data("expected segment link"));
        }
        require_zero(&bytes[37..BODY_END], "segment link padding")?;
        let link = Self {
            header,
            next_segment: read_u32(bytes, 32),
            head_id: HeadId::try_from(bytes[36])?,
        };
        link.validate(geometry)?;
        Ok(link)
    }
}

/// Records that the provisioned backing grew. Idempotent by target bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackingGrowth {
    pub(crate) header: RecordHeader,
    pub(crate) previous_backing_blocks: u64,
    pub(crate) new_backing_blocks: u64,
}

impl BackingGrowth {
    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::BackingGrowth {
            return Err(invalid_data("expected backing growth record"));
        }
        self.header.validate(geometry, self.header.volume_id)?;
        if self.previous_backing_blocks != geometry.backing_blocks
            || self.new_backing_blocks <= self.previous_backing_blocks
            || self.new_backing_blocks > MAX_BACKING_BLOCKS
        {
            return Err(invalid_data("backing growth bounds are not monotonic"));
        }
        geometry.with_backing(self.new_backing_blocks)?;
        Ok(())
    }

    pub(crate) fn encode(
        &self,
        geometry: Geometry,
        bytes: &mut [u8; BLOCK_SIZE],
    ) -> io::Result<()> {
        self.validate(geometry)?;
        self.header.encode(bytes);
        bytes[32..40].copy_from_slice(&self.previous_backing_blocks.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.new_backing_blocks.to_le_bytes());
        seal_metadata_record(bytes);
        Ok(())
    }

    /// `geometry` is the bound in force before this record.
    pub(crate) fn decode(bytes: &[u8], geometry: Geometry, volume_id: u64) -> io::Result<Self> {
        let (header, bytes) = decode_metadata_record(bytes, geometry, volume_id)?;
        if header.kind != RecordKind::BackingGrowth {
            return Err(invalid_data("expected backing growth record"));
        }
        require_zero(&bytes[48..BODY_END], "backing growth padding")?;
        let growth = Self {
            header,
            previous_backing_blocks: read_u64(bytes, 32),
            new_backing_blocks: read_u64(bytes, 40),
        };
        growth.validate(geometry)?;
        Ok(growth)
    }
}

/// LBA ranges the client released. Replay unmaps them in LSN order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LbaDiscard {
    pub(crate) header: RecordHeader,
    pub(crate) range_count: u32,
    pub(crate) start_lbas: [u32; DISCARD_RANGES],
    pub(crate) block_counts: [u32; DISCARD_RANGES],
}

impl LbaDiscard {
    pub(crate) fn empty(header: RecordHeader) -> Self {
        Self {
            header,
            range_count: 0,
            start_lbas: [0; DISCARD_RANGES],
            block_counts: [0; DISCARD_RANGES],
        }
    }

    /// Adds one range; returns false when the record is full.
    pub(crate) fn push(&mut self, start_lba: u32, block_count: u32) -> bool {
        let index = self.range_count as usize;
        if index >= DISCARD_RANGES {
            return false;
        }
        self.start_lbas[index] = start_lba;
        self.block_counts[index] = block_count;
        self.range_count += 1;
        true
    }

    pub(crate) fn ranges(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.start_lbas
            .iter()
            .zip(&self.block_counts)
            .take(self.range_count as usize)
            .map(|(&start, &count)| (start, count))
    }

    fn validate(&self, geometry: Geometry) -> io::Result<()> {
        if self.header.kind != RecordKind::LbaDiscard {
            return Err(invalid_data("expected discard record"));
        }
        self.header.validate(geometry, self.header.volume_id)?;
        if !(1..=DISCARD_RANGES as u32).contains(&self.range_count) {
            return Err(invalid_data("discard range count out of bounds"));
        }
        for (index, (start, count)) in self.start_lbas.iter().zip(&self.block_counts).enumerate() {
            if index >= self.range_count as usize {
                if *start != 0 || *count != 0 {
                    return Err(invalid_data("nonzero unused discard range"));
                }
                continue;
            }
            if *count == 0
                || *start > geometry.volume_blocks
                || *count > geometry.volume_blocks - *start
            {
                return Err(invalid_data("discard range outside the volume"));
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
        bytes[32..36].copy_from_slice(&self.range_count.to_le_bytes());
        for index in 0..DISCARD_RANGES {
            let start = DISCARD_START_OFFSET + index * 4;
            let count = DISCARD_COUNT_OFFSET + index * 4;
            bytes[start..start + 4].copy_from_slice(&self.start_lbas[index].to_le_bytes());
            bytes[count..count + 4].copy_from_slice(&self.block_counts[index].to_le_bytes());
        }
        seal_metadata_record(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], geometry: Geometry, volume_id: u64) -> io::Result<Self> {
        let (header, bytes) = decode_metadata_record(bytes, geometry, volume_id)?;
        if header.kind != RecordKind::LbaDiscard {
            return Err(invalid_data("expected discard record"));
        }
        require_zero(&bytes[36..40], "discard padding")?;
        let mut discard = Self::empty(header);
        discard.range_count = read_u32(bytes, 32);
        for index in 0..DISCARD_RANGES {
            discard.start_lbas[index] = read_u32(bytes, DISCARD_START_OFFSET + index * 4);
            discard.block_counts[index] = read_u32(bytes, DISCARD_COUNT_OFFSET + index * 4);
        }
        discard.validate(geometry)?;
        Ok(discard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v4::NO_PREVIOUS_BOUNDARY;

    fn geometry() -> Geometry {
        Geometry::new(1000, 2 + 6 * 16, 16).unwrap()
    }

    fn header(kind: RecordKind, boundary: u32) -> RecordHeader {
        RecordHeader {
            kind,
            volume_id: 31,
            lsn: 12,
            previous_boundary: NO_PREVIOUS_BOUNDARY,
            boundary,
        }
    }

    #[test]
    fn segment_link_round_trips_and_rejects_self_and_out_of_range_targets() {
        let geometry = geometry();
        let link = SegmentLink {
            header: header(RecordKind::SegmentLink, 2 + 16),
            next_segment: 4,
            head_id: HeadId::Relocation,
        };
        let mut bytes = [0xff; BLOCK_SIZE];
        link.encode(geometry, &mut bytes).unwrap();
        assert_eq!(bytes[5], 4);
        assert_eq!(read_u32(&bytes, 32), 4);
        assert_eq!(bytes[36], 1);
        assert!(bytes[37..4072].iter().all(|byte| *byte == 0));
        assert_eq!(SegmentLink::decode(&bytes, geometry, 31).unwrap(), link);
        assert!(SegmentLink::decode(&bytes, geometry, 32).is_err());
        for (offset, value) in [(32, 6), (32, 1), (36, 2), (37, 1), (4000, 1)] {
            let mut damaged = bytes;
            damaged[offset] = value;
            seal_metadata_record(&mut damaged);
            assert!(
                SegmentLink::decode(&damaged, geometry, 31).is_err(),
                "{offset}"
            );
        }
        let mut self_link = link;
        self_link.next_segment = 1;
        assert!(self_link.encode(geometry, &mut bytes).is_err());
        let mut wide = link;
        wide.header.boundary += 2;
        wide.header.previous_boundary = 2 + 16;
        assert!(wide.encode(geometry, &mut bytes).is_err());
        let mut following = link;
        following.header.boundary += 1;
        following.header.previous_boundary = 2 + 16;
        assert!(following.encode(geometry, &mut bytes).is_ok());
    }

    #[test]
    fn backing_growth_is_monotonic_and_validated_beyond_the_header_checksum() {
        let geometry = geometry();
        let growth = BackingGrowth {
            header: header(RecordKind::BackingGrowth, 2 + 16),
            previous_backing_blocks: 2 + 6 * 16,
            new_backing_blocks: 2 + 9 * 16 + 5,
        };
        let mut bytes = [0xff; BLOCK_SIZE];
        growth.encode(geometry, &mut bytes).unwrap();
        assert_eq!(bytes[5], 5);
        assert_eq!(read_u64(&bytes, 32), 2 + 6 * 16);
        assert_eq!(read_u64(&bytes, 40), 2 + 9 * 16 + 5);
        assert!(bytes[48..4072].iter().all(|byte| *byte == 0));
        assert_eq!(BackingGrowth::decode(&bytes, geometry, 31).unwrap(), growth);
        // The new bound lies outside the 40-byte header checksum; body damage must still fail.
        let mut damaged = bytes;
        damaged[40] ^= 1;
        assert!(BackingGrowth::decode(&damaged, geometry, 31).is_err());
        assert!(crate::format_v4::decode_metadata_framing(&damaged, geometry, 31).is_ok());
        for (previous, new) in [
            (2 + 6 * 16, 2 + 6 * 16),
            (2 + 6 * 16, 2 + 5 * 16),
            (2 + 5 * 16, 2 + 9 * 16),
            (2 + 6 * 16, MAX_BACKING_BLOCKS + 1),
        ] {
            let mut invalid = growth;
            invalid.previous_backing_blocks = previous;
            invalid.new_backing_blocks = new;
            assert!(
                invalid.encode(geometry, &mut bytes).is_err(),
                "{previous} {new}"
            );
        }
        let grown = geometry.with_backing(growth.new_backing_blocks).unwrap();
        assert!(BackingGrowth::decode(&bytes, grown, 31).is_err());
    }

    #[test]
    fn discard_ranges_round_trip_at_both_ends_and_reject_invalid_ranges() {
        let geometry = geometry();
        let mut discard = LbaDiscard::empty(header(RecordKind::LbaDiscard, 2 + 16));
        assert!(discard.push(0, 1));
        assert!(discard.push(999, 1));
        assert!(discard.push(10, 500));
        let mut bytes = [0xff; BLOCK_SIZE];
        discard.encode(geometry, &mut bytes).unwrap();
        assert_eq!(bytes[5], 7);
        assert_eq!(read_u32(&bytes, 32), 3);
        assert_eq!(&bytes[36..40], &[0, 0, 0, 0]);
        assert_eq!(read_u32(&bytes, 40 + 4), 999);
        assert_eq!(read_u32(&bytes, 2056 + 8), 500);
        assert!(bytes[40 + 12..2056].iter().all(|byte| *byte == 0));
        assert!(bytes[2056 + 12..4072].iter().all(|byte| *byte == 0));
        let decoded = LbaDiscard::decode(&bytes, geometry, 31).unwrap();
        assert_eq!(decoded, discard);
        assert_eq!(
            decoded.ranges().collect::<Vec<_>>(),
            [(0, 1), (999, 1), (10, 500)]
        );
        let mut full = LbaDiscard::empty(header(RecordKind::LbaDiscard, 2 + 16));
        for index in 0..DISCARD_RANGES {
            assert!(full.push(index as u32, 1));
        }
        assert!(!full.push(0, 1));
        full.encode(geometry, &mut bytes).unwrap();
        assert_eq!(read_u32(&bytes, 40 + 503 * 4), 503);
        assert_eq!(LbaDiscard::decode(&bytes, geometry, 31).unwrap(), full);
        for (offset, value) in [(32, 0), (32, 4), (36, 1), (40 + 12, 1), (2056 + 12, 1)] {
            let mut damaged = bytes;
            discard.encode(geometry, &mut damaged).unwrap();
            damaged[offset] = value;
            seal_metadata_record(&mut damaged);
            assert!(
                LbaDiscard::decode(&damaged, geometry, 31).is_err(),
                "{offset}"
            );
        }
        for (start, count) in [(0, 0), (1000, 1), (999, 2), (u32::MAX, 1)] {
            let mut invalid = LbaDiscard::empty(header(RecordKind::LbaDiscard, 2 + 16));
            invalid.push(start, count);
            assert!(
                invalid.encode(geometry, &mut bytes).is_err(),
                "{start} {count}"
            );
        }
        let empty = LbaDiscard::empty(header(RecordKind::LbaDiscard, 2 + 16));
        assert!(empty.encode(geometry, &mut bytes).is_err());
    }
}
