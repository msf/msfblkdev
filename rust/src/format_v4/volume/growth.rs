use super::*;
use crate::format_v4::{RecordKind, small_records::BackingGrowth};
use std::io::{Seek, SeekFrom};

impl Volume {
    /// The backing file's current length, which an operator may have grown.
    pub fn backing_len(&self) -> io::Result<u64> {
        Ok(self.backing.metadata()?.len())
    }

    /// Claims an externally grown backing up to `new_backing_bytes`. Idempotent by target
    /// bound: a retry at the recovered bound succeeds without another record.
    pub fn grow_backing(&mut self, new_backing_bytes: u64) -> io::Result<()> {
        self.check_operational()?;
        if !new_backing_bytes.is_multiple_of(BLOCK_SIZE as u64) {
            return Err(invalid_input("backing size is not block aligned"));
        }
        let new_blocks = new_backing_bytes / BLOCK_SIZE as u64;
        let geometry = self.log.geometry;
        if new_blocks == geometry.backing_blocks {
            return Ok(());
        }
        if new_blocks < geometry.backing_blocks {
            return Err(invalid_input("backing cannot shrink"));
        }
        let actual = self.backing.try_clone()?.seek(SeekFrom::End(0))? / BLOCK_SIZE as u64;
        if actual < new_blocks {
            return Err(invalid_input(
                "actual backing is shorter than the requested bound",
            ));
        }
        let grown = geometry.with_backing(new_blocks)?;
        if grown.segment_count > self.log.segments.capacity() {
            return Err(invalid_input(
                "backing growth exceeds the preallocated segment state",
            ));
        }
        if u64::from(grown.table_pages) + 1 > u64::from(grown.blocks_per_segment) {
            return Err(invalid_input(
                "a segment must hold every table page plus its link",
            ));
        }
        let result = self.record_growth(new_blocks, grown);
        self.latch(result)
    }

    fn record_growth(&mut self, new_blocks: u64, grown: Geometry) -> io::Result<()> {
        let previous = self.log.geometry.backing_blocks;
        // The record itself must fit in the old provisioned range.
        self.log.append_metadata(
            &mut self.backing_io,
            &self.backing,
            HeadId::User,
            RecordKind::BackingGrowth,
            |log, header, bytes| {
                BackingGrowth {
                    header,
                    previous_backing_blocks: previous,
                    new_backing_blocks: new_blocks,
                }
                .encode(log.geometry, bytes)
            },
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("growth-write-complete")?;
        storage::sync(&mut self.backing_io, &self.backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("growth-fsync-complete")?;
        self.log.segments.grow(grown)?;
        self.log.geometry = grown;
        self.actual_backing_blocks = self.actual_backing_blocks.max(new_blocks);
        self.reserve =
            Reserve::compute(grown, self.mapping.cache.snapshot().sizing.capacity as u64);
        Ok(())
    }
}
