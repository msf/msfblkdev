//! Deterministic `SplitMix64` streams for payloads and workloads. Not cryptographic.

use std::num::NonZeroU64;

// Steele, Lea and Flood, "Fast splittable pseudorandom number generators", OOPSLA 2014.
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// The `SplitMix64` finalizer: a bijective 64-bit mix.
pub fn mix64(value: u64) -> u64 {
    let mut mixed = value;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}

#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN_GAMMA);
        mix64(self.state)
    }

    /// A value in `0..bound`. The modulo bias is negligible for the bounds we use.
    pub fn below(&mut self, bound: NonZeroU64) -> u64 {
        self.next_u64() % bound
    }
}
