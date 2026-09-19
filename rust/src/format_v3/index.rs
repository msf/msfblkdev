use super::{
    checkpoint::{CheckpointDescriptor, CheckpointSlot},
    geometry::Geometry,
};
use crate::{BLOCK_SIZE, invalid_data};
use std::{io, ops::Range};
use xxhash_rust::xxh3::Xxh3;

pub(crate) struct Checkpoint {
    pub(crate) descriptor: CheckpointDescriptor,
    pub(crate) mapping_page_block: Box<[u32]>,
}

impl Checkpoint {
    pub(crate) fn empty(geometry: Geometry, volume_id: u64) -> io::Result<Self> {
        let mut index = Vec::new();
        index
            .try_reserve_exact(geometry.page_count as usize)
            .map_err(io::Error::other)?;
        index.resize(geometry.page_count as usize, 0);
        let mut checkpoint = Self {
            descriptor: CheckpointDescriptor {
                slot: CheckpointSlot::Green,
                volume_id,
                volume_blocks: geometry.volume_blocks,
                backing_blocks: geometry.backing_blocks,
                checkpoint_lsn: 0,
                last_record_boundary: geometry.log_start - 1,
                index_checksum: 0,
                generation: 0,
            },
            mapping_page_block: index.into_boxed_slice(),
        };
        checkpoint.descriptor.index_checksum = checkpoint.checksum()?;
        Ok(checkpoint)
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        let mut index = Vec::new();
        index
            .try_reserve_exact(self.mapping_page_block.len())
            .map_err(io::Error::other)?;
        index.extend_from_slice(&self.mapping_page_block);
        Ok(Self {
            descriptor: self.descriptor,
            mapping_page_block: index.into_boxed_slice(),
        })
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        std::mem::size_of_val(self.mapping_page_block.as_ref())
    }

    fn block_range(&self, block: u32) -> io::Result<Range<usize>> {
        let geometry = self.descriptor.geometry()?;
        if self.mapping_page_block.len() != geometry.page_count as usize
            || block >= geometry.index_blocks
        {
            return Err(invalid_data("invalid checkpoint index shape or block"));
        }
        let start = block as usize * (BLOCK_SIZE / size_of::<u32>());
        Ok(start..(start + BLOCK_SIZE / size_of::<u32>()).min(self.mapping_page_block.len()))
    }

    fn validate_pointer(&self, pointer: u32, geometry: Geometry) -> io::Result<()> {
        if pointer != 0
            && (pointer < geometry.log_start
                || pointer > self.descriptor.last_record_boundary
                || u64::from(pointer) >= geometry.backing_blocks)
        {
            return Err(invalid_data("mapping-page pointer outside checkpoint log"));
        }
        Ok(())
    }

    pub(crate) fn encode_block(&self, block: u32, bytes: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        let range = self.block_range(block)?;
        let geometry = self.descriptor.geometry()?;
        bytes.fill(0);
        for (pointer, encoded) in self.mapping_page_block[range]
            .iter()
            .zip(bytes.chunks_exact_mut(4))
        {
            self.validate_pointer(*pointer, geometry)?;
            encoded.copy_from_slice(&pointer.to_le_bytes());
        }
        Ok(())
    }

    pub(crate) fn decode_block(&mut self, block: u32, bytes: &[u8]) -> io::Result<()> {
        let range = self.block_range(block)?;
        let geometry = self.descriptor.geometry()?;
        if bytes.len() != BLOCK_SIZE || bytes[range.len() * 4..].iter().any(|byte| *byte != 0) {
            return Err(invalid_data("checkpoint index length or padding mismatch"));
        }
        for encoded in bytes.chunks_exact(4).take(range.len()) {
            self.validate_pointer(
                u32::from_le_bytes(encoded.try_into().expect("four-byte chunk")),
                geometry,
            )?;
        }
        for (pointer, encoded) in self.mapping_page_block[range]
            .iter_mut()
            .zip(bytes.chunks_exact(4))
        {
            *pointer = u32::from_le_bytes(encoded.try_into().expect("four-byte chunk"));
        }
        Ok(())
    }

    pub(crate) fn checksum(&self) -> io::Result<u64> {
        let mut hasher = Xxh3::new();
        let mut bytes = [0; BLOCK_SIZE];
        for block in 0..self.descriptor.geometry()?.index_blocks {
            self.encode_block(block, &mut bytes)?;
            hasher.update(&bytes);
        }
        Ok(hasher.digest())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> Checkpoint {
        let geometry = Geometry::new(336 * 1024 + 1, 1000).unwrap();
        let mut checkpoint = Checkpoint::empty(geometry, 7).unwrap();
        checkpoint.descriptor.checkpoint_lsn = 3;
        checkpoint.descriptor.last_record_boundary = 8;
        checkpoint.descriptor.generation = 1;
        checkpoint.mapping_page_block[0] = 6;
        checkpoint.mapping_page_block[1024] = 8;
        checkpoint.descriptor.index_checksum = checkpoint.checksum().unwrap();
        checkpoint
    }

    #[test]
    fn complete_checkpoint_owns_and_streams_padded_pointer_array() {
        let checkpoint = populated();
        assert_eq!(checkpoint.allocated_bytes(), 1025 * 4);
        let mut independently_encoded = [0; 8192];
        independently_encoded[..4].copy_from_slice(&6_u32.to_le_bytes());
        independently_encoded[4096..4100].copy_from_slice(&8_u32.to_le_bytes());
        assert_eq!(
            checkpoint.descriptor.index_checksum,
            xxhash_rust::xxh3::xxh3_64(&independently_encoded)
        );
        let mut recovered = checkpoint.try_clone().unwrap();
        recovered.mapping_page_block.fill(0);
        let mut bytes = [0xff; BLOCK_SIZE];
        for block in 0..2 {
            checkpoint.encode_block(block, &mut bytes).unwrap();
            assert_eq!(
                &bytes,
                &independently_encoded[block as usize * 4096..(block as usize + 1) * 4096]
            );
            recovered.decode_block(block, &bytes).unwrap();
        }
        assert_eq!(recovered.mapping_page_block, checkpoint.mapping_page_block);
        assert_eq!(
            recovered.checksum().unwrap(),
            checkpoint.descriptor.index_checksum
        );
        let empty = Checkpoint::empty(Geometry::new(1, 7).unwrap(), 19).unwrap();
        assert_eq!(empty.descriptor.index_checksum, 0x93d7_6fe1_48c6_89ba);
        assert_eq!(empty.descriptor.last_record_boundary, 3);
        assert_eq!(empty.descriptor.generation, 0);
    }

    #[test]
    fn invalid_index_blocks_do_not_mutate_the_candidate() {
        let mut checkpoint = populated();
        let original = checkpoint.mapping_page_block.clone();
        for pointer in [1_u32, 5, 9, 1000, u32::MAX] {
            let mut bytes = [0; BLOCK_SIZE];
            bytes[..4].copy_from_slice(&pointer.to_le_bytes());
            assert!(checkpoint.decode_block(1, &bytes).is_err());
            assert_eq!(checkpoint.mapping_page_block, original);
        }
        for offset in [4, 4095] {
            let mut bytes = [0; BLOCK_SIZE];
            bytes[offset] = 1;
            assert!(checkpoint.decode_block(1, &bytes).is_err());
        }
        for length in [0, 4, 4095, 4097] {
            assert!(checkpoint.decode_block(0, &vec![0; length]).is_err());
        }
        let mut bytes = [0; BLOCK_SIZE];
        assert!(checkpoint.decode_block(2, &bytes).is_err());
        assert!(checkpoint.encode_block(u32::MAX, &mut bytes).is_err());
        checkpoint.mapping_page_block[0] = 1;
        assert!(checkpoint.encode_block(0, &mut bytes).is_err());
        checkpoint.mapping_page_block = Box::new([]);
        assert!(checkpoint.encode_block(0, &mut bytes).is_err());
    }

    #[test]
    fn streamed_index_work_allocates_nothing() {
        let checkpoint = populated();
        let mut recovered = checkpoint.try_clone().unwrap();
        let (_, allocations) = crate::tests::allocations::measure(|| {
            let mut bytes = [0; BLOCK_SIZE];
            for block in 0..2 {
                checkpoint.encode_block(block, &mut bytes).unwrap();
                recovered.decode_block(block, &bytes).unwrap();
            }
            assert_eq!(
                checkpoint.checksum().unwrap(),
                recovered.checksum().unwrap()
            );
        });
        assert_eq!(allocations, 0);
    }
}
