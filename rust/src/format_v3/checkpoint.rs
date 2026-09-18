use super::{CHECKSUM_OFFSET, VERSION, geometry::Geometry, seal_metadata, validate_metadata};
use crate::{BLOCK_SIZE, invalid_data, read_u32, read_u64};
use std::io;

const MAGIC: &[u8; 4] = b"VBLC";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CheckpointSlot {
    Green = 0,
    Blue = 1,
}

impl CheckpointSlot {
    pub(crate) fn other(self) -> Self {
        match self {
            Self::Green => Self::Blue,
            Self::Blue => Self::Green,
        }
    }

    pub(crate) fn body_start(self, geometry: Geometry) -> u32 {
        match self {
            Self::Green => 2,
            Self::Blue => 2 + geometry.index_blocks,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CheckpointDescriptor {
    pub(crate) slot: CheckpointSlot,
    pub(crate) volume_id: u64,
    pub(crate) volume_blocks: u32,
    pub(crate) backing_blocks: u64,
    pub(crate) checkpoint_lsn: u64,
    pub(crate) last_record_boundary: u32,
    pub(crate) index_checksum: u64,
    pub(crate) generation: u64,
}

impl CheckpointDescriptor {
    pub(crate) fn geometry(self) -> io::Result<Geometry> {
        Geometry::new(self.volume_blocks, self.backing_blocks)
    }

    fn validate(self) -> io::Result<()> {
        let geometry = self.geometry()?;
        let initial = self.checkpoint_lsn == 0;
        if self.last_record_boundary < geometry.log_start - 1
            || u64::from(self.last_record_boundary) >= geometry.backing_blocks
            || initial != (self.last_record_boundary == geometry.log_start - 1)
            || (self.generation == 0 && !initial)
            || self.checkpoint_lsn > u64::from(self.last_record_boundary - (geometry.log_start - 1))
        {
            return Err(invalid_data("invalid checkpoint sequence or boundary"));
        }
        Ok(())
    }

    pub(crate) fn next_generation(self) -> io::Result<u64> {
        self.generation
            .checked_add(1)
            .ok_or_else(|| invalid_data("checkpoint generation exhausted"))
    }

    pub(crate) fn validate_pair(self, other: Self) -> io::Result<()> {
        if self.volume_id != other.volume_id
            || self.volume_blocks != other.volume_blocks
            || self.backing_blocks != other.backing_blocks
        {
            return Err(invalid_data(
                "checkpoint roots disagree on identity or geometry",
            ));
        }
        if self.generation == other.generation
            && (self.checkpoint_lsn != other.checkpoint_lsn
                || self.last_record_boundary != other.last_record_boundary
                || self.index_checksum != other.index_checksum)
        {
            return Err(invalid_data("equal-generation checkpoints disagree"));
        }
        Ok(())
    }

    pub(crate) fn encode(self, bytes: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        self.validate()?;
        bytes.fill(0);
        bytes[..4].copy_from_slice(MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.slot as u8;
        bytes[8..16].copy_from_slice(&self.volume_id.to_le_bytes());
        bytes[16..20].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        bytes[20..24].copy_from_slice(&self.volume_blocks.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.backing_blocks.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.checkpoint_lsn.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.last_record_boundary.to_le_bytes());
        bytes[44..52].copy_from_slice(&self.index_checksum.to_le_bytes());
        bytes[52..60].copy_from_slice(&self.generation.to_le_bytes());
        seal_metadata(bytes);
        Ok(())
    }

    pub(crate) fn decode(
        bytes: &[u8],
        slot: CheckpointSlot,
        backing_blocks: u64,
    ) -> io::Result<Self> {
        let bytes = validate_metadata(bytes, MAGIC)?;
        if bytes[5] != slot as u8
            || bytes[6..8] != [0, 0]
            || read_u32(bytes, 16) != BLOCK_SIZE as u32
            || read_u64(bytes, 24) != backing_blocks
            || bytes[60..CHECKSUM_OFFSET].iter().any(|byte| *byte != 0)
        {
            return Err(invalid_data(
                "checkpoint slot, geometry, or padding mismatch",
            ));
        }
        let descriptor = Self {
            slot,
            volume_id: read_u64(bytes, 8),
            volume_blocks: read_u32(bytes, 20),
            backing_blocks,
            checkpoint_lsn: read_u64(bytes, 32),
            last_record_boundary: read_u32(bytes, 40),
            index_checksum: read_u64(bytes, 44),
            generation: read_u64(bytes, 52),
        };
        descriptor.validate()?;
        Ok(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> CheckpointDescriptor {
        CheckpointDescriptor {
            slot: CheckpointSlot::Blue,
            volume_id: 19,
            volume_blocks: 1000,
            backing_blocks: 1000,
            checkpoint_lsn: 2,
            last_record_boundary: 6,
            index_checksum: 0x0102_0304_0506_0708,
            generation: 1,
        }
    }

    #[test]
    fn descriptor_fields_have_exact_offsets_and_checksum() {
        let descriptor = descriptor();
        let mut bytes = [0xff; BLOCK_SIZE];
        descriptor.encode(&mut bytes).unwrap();
        assert_eq!(&bytes[..8], &[b'V', b'B', b'L', b'C', 3, 1, 0, 0]);
        assert_eq!(read_u64(&bytes, 8), 19);
        assert_eq!(read_u32(&bytes, 16), 4096);
        assert_eq!(read_u32(&bytes, 20), 1000);
        assert_eq!(read_u64(&bytes, 24), 1000);
        assert_eq!(read_u64(&bytes, 32), 2);
        assert_eq!(read_u32(&bytes, 40), 6);
        assert_eq!(&bytes[44..52], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(read_u64(&bytes, 52), 1);
        assert!(bytes[60..4088].iter().all(|byte| *byte == 0));
        let mut independent = bytes;
        independent[4088..].fill(0);
        assert_eq!(
            read_u64(&bytes, 4088),
            xxhash_rust::xxh3::xxh3_64(&independent)
        );
        assert_eq!(
            CheckpointDescriptor::decode(&bytes, CheckpointSlot::Blue, 1000).unwrap(),
            descriptor
        );
        assert_eq!(
            descriptor.slot.body_start(descriptor.geometry().unwrap()),
            3
        );
        assert_eq!(
            descriptor
                .slot
                .other()
                .body_start(descriptor.geometry().unwrap()),
            2
        );
        assert_eq!(descriptor.slot.other().other(), descriptor.slot);
    }

    #[test]
    fn descriptor_rejects_malformed_geometry_sequence_padding_and_versions() {
        let mut original = [0; BLOCK_SIZE];
        descriptor().encode(&mut original).unwrap();
        for (offset, value) in [
            (4, 1),
            (4, 2),
            (4, 4),
            (5, 0),
            (5, 2),
            (6, 1),
            (16, 1),
            (32, 0),
            (32, 4),
            (40, 2),
            (40, 3),
            (52, 0),
            (60, 1),
            (4087, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            seal_metadata(&mut bytes);
            assert!(
                CheckpointDescriptor::decode(&bytes, CheckpointSlot::Blue, 1000).is_err(),
                "offset={offset}"
            );
        }
        for length in [0, 60, 4095] {
            assert!(
                CheckpointDescriptor::decode(&original[..length], CheckpointSlot::Blue, 1000)
                    .is_err()
            );
        }
        assert!(CheckpointDescriptor::decode(&original, CheckpointSlot::Blue, 999).is_err());
        let mut initial = descriptor();
        initial.checkpoint_lsn = 0;
        initial.last_record_boundary = 3;
        initial.generation = 0;
        initial.encode(&mut original).unwrap();
        assert_eq!(
            CheckpointDescriptor::decode(&original, CheckpointSlot::Blue, 1000).unwrap(),
            initial
        );
    }

    #[test]
    fn generations_allow_equivalent_slots_and_reject_conflicts_and_overflow() {
        let blue = descriptor();
        let mut green = blue;
        green.slot = CheckpointSlot::Green;
        assert!(blue.validate_pair(green).is_ok());
        green.index_checksum ^= 1;
        assert!(blue.validate_pair(green).is_err());
        green.generation = blue.next_generation().unwrap();
        assert!(blue.validate_pair(green).is_ok());
        assert!(green.generation > blue.generation);
        green.volume_id += 1;
        assert!(blue.validate_pair(green).is_err());
        green = blue;
        green.volume_blocks += 1;
        assert!(blue.validate_pair(green).is_err());
        green = blue;
        green.backing_blocks += 1;
        assert!(blue.validate_pair(green).is_err());
        green.generation = u64::MAX;
        assert!(green.next_generation().is_err());
    }
}
