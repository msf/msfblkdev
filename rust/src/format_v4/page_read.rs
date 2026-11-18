use super::{
    log::Log,
    mapping_page::{MappingPageRecord, PageExpectation},
};
use crate::{
    AlignedBlock, BLOCK_SIZE,
    accounted_io::{BackingIo, submit_exact},
    mapping_cache::{MappingCache, MappingEntries},
};
use io_uring::{opcode, types};
use std::{fs::File, io, os::fd::AsRawFd};

// One demand page and up to three adjacent current snapshots, as in format 3.
pub(crate) const READ_AHEAD_PAGES: usize = 4;

pub(super) struct PageRead {
    blocks: [AlignedBlock; READ_AHEAD_PAGES],
    pub(super) count: usize,
}

impl PageRead {
    pub(super) fn new() -> Self {
        Self {
            blocks: [const { AlignedBlock([0; BLOCK_SIZE]) }; READ_AHEAD_PAGES],
            count: 0,
        }
    }

    /// Reads the demanded snapshot and up to `limit - 1` following ones stored in consecutive
    /// blocks. Consecutive snapshots never span a segment, so one read stays within it.
    pub(super) fn read(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        log: &Log,
        page_id: u32,
        limit: usize,
    ) -> io::Result<MappingEntries> {
        assert!((1..=READ_AHEAD_PAGES).contains(&limit));
        log.geometry.page_entries(page_id)?;
        let pointers = log.index.snapshot_blocks();
        let boundary = pointers[page_id as usize];
        if boundary == 0 {
            return Ok(MappingEntries::zeroed());
        }
        self.count = 1;
        while self.count < limit
            && pointers
                .get(page_id as usize + self.count)
                .is_some_and(|&next| u64::from(next) == u64::from(boundary) + self.count as u64)
        {
            self.count += 1;
        }
        let expected = self.count * BLOCK_SIZE;
        let submitted = expected;
        #[cfg(test)]
        let submitted = io.metadata_short_read.unwrap_or(submitted).min(expected);
        assert_eq!(size_of::<AlignedBlock>(), BLOCK_SIZE);
        let read = opcode::Read::new(
            types::Fd(backing.as_raw_fd()),
            self.blocks.as_mut_ptr().cast(),
            submitted as u32,
        )
        .offset(u64::from(boundary) * BLOCK_SIZE as u64)
        .build();
        submit_exact(io, read, u64::from(boundary), expected as i32)?;
        io.mapping_io.page_reads += self.count as u64;
        io.mapping_io.demand_page_read_ops += 1;
        self.decode(log, page_id, 0)
    }

    pub(super) fn install_prefetched(
        &self,
        cache: &mut MappingCache,
        log: &Log,
        io: &mut BackingIo,
        page_id: u32,
    ) -> io::Result<()> {
        for offset in 1..self.count {
            match self.decode(log, page_id, offset) {
                Ok(entries) => {
                    if !cache
                        .prefetch(page_id + offset as u32, entries)
                        .map_err(|error| io::Error::other(format!("mapping cache: {error:?}")))?
                    {
                        io.mapping_io.prefetch_rejected += 1;
                    }
                }
                // A speculative page cannot make an otherwise valid demand read fail.
                Err(_) => io.mapping_io.prefetch_rejected += 1,
            }
        }
        Ok(())
    }

    fn decode(&self, log: &Log, first_id: u32, offset: usize) -> io::Result<MappingEntries> {
        assert!(offset < self.count);
        let page_id = first_id + offset as u32;
        Ok(MappingPageRecord::decode(
            &self.blocks[offset].0,
            &PageExpectation {
                geometry: log.geometry,
                volume_id: log.volume_id,
                page_id,
                boundary: log.index.snapshot_block(page_id),
                // Relocated snapshots can be newer than the active root.
                maximum_lsn: log.next_lsn - 1,
            },
        )?
        .entries)
    }
}
