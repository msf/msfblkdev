//! The block interface every tested target implements.

use std::{error, fmt, io};

use crate::payload::Block;

#[derive(Debug)]
pub enum IoFailure {
    /// The target refused the request for lack of space (ENOSPC).
    NoSpace,
    /// The target does not support the request, for example discard on v1.2.3.
    Unsupported,
    Other(io::Error),
}

impl fmt::Display for IoFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSpace => formatter.write_str("no space on target"),
            Self::Unsupported => formatter.write_str("operation not supported by target"),
            Self::Other(error) => write!(formatter, "target I/O failed: {error}"),
        }
    }
}

impl error::Error for IoFailure {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Other(error) => Some(error),
            Self::NoSpace | Self::Unsupported => None,
        }
    }
}

/// A device of 4 KiB logical blocks.
pub trait BlockTarget {
    fn block_count(&self) -> u64;

    /// # Errors
    /// Returns the target's failure; the block's contents are then unspecified.
    fn read(&mut self, lba: u64, block: &mut Block) -> Result<(), IoFailure>;

    /// # Errors
    /// Returns the target's failure. After `NoSpace` the target must be unchanged.
    fn write(&mut self, lba: u64, block: &Block) -> Result<(), IoFailure>;

    /// # Errors
    /// Returns the target's failure; earlier writes are then not known to be durable.
    fn flush(&mut self) -> Result<(), IoFailure>;

    /// # Errors
    /// Returns the target's failure, including `Unsupported`.
    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), IoFailure>;
}
