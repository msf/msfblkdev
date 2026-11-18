//! 4 KiB payloads derived from (seed, LBA, version).
//!
//! Word 0 holds the version masked by a value from the seed and LBA. The other words come from a
//! keyed `SplitMix64` stream. A block written for another LBA or seed does not decode.

use std::fmt;
use std::num::NonZeroU32;

use crate::rng::{SplitMix64, mix64};

pub const BLOCK_SIZE: usize = 4096;
const WORD_BYTES: usize = 8;
const _: () = assert!(
    BLOCK_SIZE.is_multiple_of(WORD_BYTES),
    "a block holds whole words"
);

/// One 4 KiB block, aligned for `O_DIRECT`.
#[derive(Clone)]
#[repr(C, align(4096))]
pub struct Block(pub [u8; BLOCK_SIZE]);

impl Block {
    pub const fn zeroed() -> Self {
        Self([0; BLOCK_SIZE])
    }

    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|&byte| byte == 0)
    }
}

impl fmt::Debug for Block {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Block(..)")
    }
}

/// What a block read back from a target contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    Zero,
    Version(NonZeroU32),
    Invalid,
}

#[derive(Clone, Copy, Debug)]
pub struct PayloadCodec {
    seed: u64,
}

impl PayloadCodec {
    pub const fn new(seed: u64) -> Self {
        Self { seed }
    }

    pub fn fill(self, block: &mut Block, lba: u64, version: NonZeroU32) {
        let mut stream = SplitMix64::new(self.stream_seed(lba, version));
        let masked = u64::from(version.get()) ^ self.mask(lba);
        let (words, _) = block.0.as_chunks_mut::<WORD_BYTES>();
        let Some((first, rest)) = words.split_first_mut() else {
            return;
        };
        *first = masked.to_le_bytes();
        for word in rest {
            *word = stream.next_u64().to_le_bytes();
        }
    }

    pub fn decode(self, block: &Block, lba: u64) -> Content {
        if block.is_zero() {
            return Content::Zero;
        }
        let (words, _) = block.0.as_chunks::<WORD_BYTES>();
        let Some((first, rest)) = words.split_first() else {
            return Content::Invalid;
        };
        let unmasked = u64::from_le_bytes(*first) ^ self.mask(lba);
        let Some(version) = u32::try_from(unmasked).ok().and_then(NonZeroU32::new) else {
            return Content::Invalid;
        };
        let mut stream = SplitMix64::new(self.stream_seed(lba, version));
        let matches = rest
            .iter()
            .all(|word| *word == stream.next_u64().to_le_bytes());
        if matches {
            Content::Version(version)
        } else {
            Content::Invalid
        }
    }

    fn mask(self, lba: u64) -> u64 {
        mix64(self.seed ^ mix64(lba))
    }

    fn stream_seed(self, lba: u64, version: NonZeroU32) -> u64 {
        mix64(self.mask(lba) ^ mix64(!u64::from(version.get())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    #[test]
    fn round_trips_its_own_payload() {
        let codec = PayloadCodec::new(7);
        let mut block = Block::zeroed();
        codec.fill(&mut block, 42, version(9));
        assert_eq!(codec.decode(&block, 42), Content::Version(version(9)));
    }

    #[test]
    fn rejects_other_lba_seed_and_damage() {
        let codec = PayloadCodec::new(7);
        let mut block = Block::zeroed();
        codec.fill(&mut block, 42, version(9));
        assert_eq!(codec.decode(&block, 43), Content::Invalid);
        assert_eq!(PayloadCodec::new(8).decode(&block, 42), Content::Invalid);
        block.0[4000] ^= 1;
        assert_eq!(codec.decode(&block, 42), Content::Invalid);
    }

    #[test]
    fn zero_block_is_zero() {
        assert_eq!(
            PayloadCodec::new(7).decode(&Block::zeroed(), 1),
            Content::Zero
        );
    }
}
