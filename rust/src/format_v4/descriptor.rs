use super::{
    CHECKSUM_OFFSET, DESCRIPTOR_MAGIC, Geometry, HeadId, VERSION,
    geometry::{DIRECT_POINTERS, INDIRECT_POINTERS, TABLE_POINTERS},
    require_zero, seal_block, validate_block_checksum,
};
use crate::{BLOCK_SIZE, invalid_data, read_u32, read_u64};
use std::io;

const DIRECT_OFFSET: usize = 60;
const INDIRECT_OFFSET: usize = DIRECT_OFFSET + DIRECT_POINTERS * 4;
const TABLE_COUNT_OFFSET: usize = INDIRECT_OFFSET + INDIRECT_POINTERS * 4;
const TABLE_OFFSET: usize = TABLE_COUNT_OFFSET + 4;
const PADDING_OFFSET: usize = TABLE_OFFSET + TABLE_POINTERS * 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CheckpointSlot {
    Green = 0,
    Blue = 1,
}

impl CheckpointSlot {
    pub(crate) const BOTH: [Self; 2] = [Self::Green, Self::Blue];

    pub(crate) fn other(self) -> Self {
        match self {
            Self::Green => Self::Blue,
            Self::Blue => Self::Green,
        }
    }

    pub(crate) fn block(self) -> u32 {
        self as u32
    }
}

/// One 4 KiB checkpoint root. Both index arrays always exist; unused entries are zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckpointDescriptor {
    pub(crate) slot: CheckpointSlot,
    pub(crate) volume_id: u64,
    pub(crate) volume_blocks: u32,
    pub(crate) backing_blocks: u64,
    pub(crate) checkpoint_lsn: u64,
    pub(crate) blocks_per_segment: u32,
    /// Last record boundary of User and Relocation; zero means the head has no record yet.
    pub(crate) head_boundary: [u32; 2],
    pub(crate) direct_pages: [u32; DIRECT_POINTERS],
    pub(crate) indirect_pages: [u32; INDIRECT_POINTERS],
    pub(crate) table_pages: [u32; TABLE_POINTERS],
}

impl CheckpointDescriptor {
    pub(crate) fn geometry(&self) -> io::Result<Geometry> {
        Geometry::new(
            self.volume_blocks,
            self.backing_blocks,
            self.blocks_per_segment,
        )
    }

    pub(crate) fn head_boundary(&self, head: HeadId) -> u32 {
        self.head_boundary[head.index()]
    }

    fn validate(&self) -> io::Result<()> {
        let geometry = self.geometry()?;
        if self.checkpoint_lsn == 0 || self.head_boundary(HeadId::User) == 0 {
            return Err(invalid_data(
                "every format-4 checkpoint records its initial table pages",
            ));
        }
        for boundary in self.head_boundary {
            if boundary != 0 && !geometry.contains_block(boundary) {
                return Err(invalid_data("head boundary outside every segment"));
            }
        }
        let pointer_sets: [(&[u32], usize); 3] = [
            (&self.direct_pages, geometry.direct_pages as usize),
            (&self.indirect_pages, geometry.indirect_pages as usize),
            (&self.table_pages, geometry.table_pages as usize),
        ];
        for (pointers, used) in pointer_sets {
            for (index, &pointer) in pointers.iter().enumerate() {
                if index >= used && pointer != 0 {
                    return Err(invalid_data("index pointer beyond the volume's shape"));
                }
                if pointer != 0 && !geometry.contains_block(pointer) {
                    return Err(invalid_data("index pointer outside every segment"));
                }
            }
        }
        Ok(())
    }

    /// Two valid roots must describe one volume. Equal checkpoint LSNs must be one checkpoint.
    pub(crate) fn validate_pair(&self, other: &Self) -> io::Result<()> {
        if self.volume_id != other.volume_id
            || self.volume_blocks != other.volume_blocks
            || self.blocks_per_segment != other.blocks_per_segment
        {
            return Err(invalid_data(
                "checkpoint roots disagree on identity or geometry",
            ));
        }
        if self.checkpoint_lsn == other.checkpoint_lsn {
            let mut same_slot = other.clone();
            same_slot.slot = self.slot;
            if same_slot != *self {
                return Err(invalid_data("equal-LSN checkpoints disagree"));
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&self, bytes: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        self.validate()?;
        bytes.fill(0);
        bytes[..4].copy_from_slice(DESCRIPTOR_MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.slot as u8;
        bytes[8..16].copy_from_slice(&self.volume_id.to_le_bytes());
        bytes[16..20].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        bytes[20..24].copy_from_slice(&self.volume_blocks.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.backing_blocks.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.checkpoint_lsn.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.blocks_per_segment.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.head_boundary[0].to_le_bytes());
        bytes[56..60].copy_from_slice(&self.head_boundary[1].to_le_bytes());
        encode_u32s(&mut bytes[DIRECT_OFFSET..], &self.direct_pages);
        encode_u32s(&mut bytes[INDIRECT_OFFSET..], &self.indirect_pages);
        let geometry = self.geometry()?;
        bytes[TABLE_COUNT_OFFSET..TABLE_OFFSET]
            .copy_from_slice(&geometry.table_pages.to_le_bytes());
        encode_u32s(&mut bytes[TABLE_OFFSET..], &self.table_pages);
        seal_block(bytes);
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8], slot: CheckpointSlot) -> io::Result<Self> {
        let bytes = validate_block_checksum(bytes, DESCRIPTOR_MAGIC)?;
        if bytes[5] != slot as u8 || read_u32(bytes, 16) != BLOCK_SIZE as u32 {
            return Err(invalid_data("checkpoint slot or block size mismatch"));
        }
        require_zero(&bytes[6..8], "descriptor padding")?;
        require_zero(&bytes[40..48], "descriptor padding")?;
        require_zero(
            &bytes[PADDING_OFFSET..CHECKSUM_OFFSET],
            "descriptor padding",
        )?;
        let mut descriptor = Self {
            slot,
            volume_id: read_u64(bytes, 8),
            volume_blocks: read_u32(bytes, 20),
            backing_blocks: read_u64(bytes, 24),
            checkpoint_lsn: read_u64(bytes, 32),
            blocks_per_segment: read_u32(bytes, 48),
            head_boundary: [read_u32(bytes, 52), read_u32(bytes, 56)],
            direct_pages: [0; DIRECT_POINTERS],
            indirect_pages: [0; INDIRECT_POINTERS],
            table_pages: [0; TABLE_POINTERS],
        };
        decode_u32s(&bytes[DIRECT_OFFSET..], &mut descriptor.direct_pages);
        decode_u32s(&bytes[INDIRECT_OFFSET..], &mut descriptor.indirect_pages);
        decode_u32s(&bytes[TABLE_OFFSET..], &mut descriptor.table_pages);
        descriptor.validate()?;
        if read_u32(bytes, TABLE_COUNT_OFFSET) != descriptor.geometry()?.table_pages {
            return Err(invalid_data("segment table page count mismatch"));
        }
        Ok(descriptor)
    }
}

fn encode_u32s(bytes: &mut [u8], values: &[u32]) {
    for (value, encoded) in values.iter().zip(bytes.chunks_exact_mut(4)) {
        encoded.copy_from_slice(&value.to_le_bytes());
    }
}

fn decode_u32s(bytes: &[u8], values: &mut [u32]) {
    for (value, encoded) in values.iter_mut().zip(bytes.chunks_exact(4)) {
        *value = u32::from_le_bytes(encoded.try_into().expect("four-byte chunk"));
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A descriptor with the given geometry and no index or table pointers.
    pub(crate) fn blank(geometry: Geometry) -> CheckpointDescriptor {
        CheckpointDescriptor {
            slot: CheckpointSlot::Green,
            volume_id: 3,
            volume_blocks: geometry.volume_blocks,
            backing_blocks: geometry.backing_blocks,
            checkpoint_lsn: 1,
            blocks_per_segment: geometry.blocks_per_segment,
            head_boundary: [2, 0],
            direct_pages: [0; DIRECT_POINTERS],
            indirect_pages: [0; INDIRECT_POINTERS],
            table_pages: [0; TABLE_POINTERS],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn descriptor() -> CheckpointDescriptor {
        // 12 segments of 4096 blocks, 128 MiB logical.
        let mut descriptor = CheckpointDescriptor {
            slot: CheckpointSlot::Blue,
            volume_id: 19,
            volume_blocks: 32_768,
            backing_blocks: 2 + 12 * 4096,
            checkpoint_lsn: 7,
            blocks_per_segment: 4096,
            head_boundary: [2 + 5, 0],
            direct_pages: [0; DIRECT_POINTERS],
            indirect_pages: [0; INDIRECT_POINTERS],
            table_pages: [0; TABLE_POINTERS],
        };
        descriptor.direct_pages[0] = 2 + 4;
        descriptor.table_pages[0] = 2 + 5;
        descriptor
    }

    #[test]
    fn descriptor_fields_have_exact_offsets_and_checksum() {
        let descriptor = descriptor();
        let mut bytes = [0xff; BLOCK_SIZE];
        descriptor.encode(&mut bytes).unwrap();
        assert_eq!(&bytes[..8], &[b'V', b'B', b'L', b'C', 4, 1, 0, 0]);
        assert_eq!(read_u64(&bytes, 8), 19);
        assert_eq!(read_u32(&bytes, 16), 4096);
        assert_eq!(read_u32(&bytes, 20), 32_768);
        assert_eq!(read_u64(&bytes, 24), 2 + 12 * 4096);
        assert_eq!(read_u64(&bytes, 32), 7);
        assert!(bytes[40..48].iter().all(|byte| *byte == 0));
        assert_eq!(read_u32(&bytes, 48), 4096);
        assert_eq!(read_u32(&bytes, 52), 7);
        assert_eq!(read_u32(&bytes, 56), 0);
        assert_eq!(read_u32(&bytes, 60), 6);
        assert!(bytes[64..316].iter().all(|byte| *byte == 0));
        assert!(bytes[316..348].iter().all(|byte| *byte == 0));
        assert_eq!(read_u32(&bytes, 348), 1);
        assert_eq!(read_u32(&bytes, 352), 7);
        assert!(bytes[356..4072].iter().all(|byte| *byte == 0));
        assert!(bytes[4072..4088].iter().all(|byte| *byte == 0));
        let mut independent = bytes;
        independent[4088..].fill(0);
        assert_eq!(
            read_u64(&bytes, 4088),
            xxhash_rust::xxh3::xxh3_64(&independent)
        );
        assert_eq!(
            CheckpointDescriptor::decode(&bytes, CheckpointSlot::Blue).unwrap(),
            descriptor
        );
        assert!(CheckpointDescriptor::decode(&bytes, CheckpointSlot::Green).is_err());
        assert_eq!(descriptor.head_boundary(HeadId::User), 7);
        assert_eq!(descriptor.head_boundary(HeadId::Relocation), 0);
        assert_eq!(CheckpointSlot::Green.other(), CheckpointSlot::Blue);
        assert_eq!(CheckpointSlot::Blue.block(), 1);
    }

    #[test]
    fn descriptor_rejects_malformed_fields_without_panic() {
        let mut original = [0; BLOCK_SIZE];
        descriptor().encode(&mut original).unwrap();
        for (offset, value) in [
            (4, 3),
            (4, 5),
            (5, 0),
            (5, 2),
            (6, 1),
            (16, 1),
            (32, 0),
            (40, 1),
            (49, 0),
            (52, 0),
            (52, 1),
            (56, 1),
            (60, 1),
            (64, 6),
            (316, 6),
            (348, 2),
            (356, 6),
            (4072, 1),
            (4087, 1),
        ] {
            let mut bytes = original;
            bytes[offset] = value;
            seal_block(&mut bytes);
            assert!(
                CheckpointDescriptor::decode(&bytes, CheckpointSlot::Blue).is_err(),
                "offset={offset} value={value}"
            );
        }
        // A pointer past the segment area, a boundary in the partial tail, and a lower bound.
        for (offset, value) in [(60_usize, 2 + 12 * 4096_u32), (52, 2 + 12 * 4096), (352, 1)] {
            let mut bytes = original;
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            seal_block(&mut bytes);
            assert!(CheckpointDescriptor::decode(&bytes, CheckpointSlot::Blue).is_err());
        }
        for length in [0, 60, 4095] {
            assert!(
                CheckpointDescriptor::decode(&original[..length], CheckpointSlot::Blue).is_err()
            );
        }
        original[100] ^= 1;
        assert!(CheckpointDescriptor::decode(&original, CheckpointSlot::Blue).is_err());
    }

    #[test]
    fn equal_lsn_roots_must_match_and_growth_may_differ() {
        let blue = descriptor();
        let mut green = blue.clone();
        green.slot = CheckpointSlot::Green;
        assert!(blue.validate_pair(&green).is_ok());
        green.table_pages[0] = 6;
        assert!(blue.validate_pair(&green).is_err());
        green = blue.clone();
        green.slot = CheckpointSlot::Green;
        green.checkpoint_lsn += 1;
        green.backing_blocks += 4096;
        assert!(blue.validate_pair(&green).is_ok());
        green.volume_blocks += 1;
        assert!(blue.validate_pair(&green).is_err());
        green = blue.clone();
        green.blocks_per_segment *= 2;
        assert!(blue.validate_pair(&green).is_err());
        green = blue.clone();
        green.volume_id += 1;
        assert!(blue.validate_pair(&green).is_err());
    }
}
