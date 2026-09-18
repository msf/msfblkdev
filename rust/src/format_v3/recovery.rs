use super::{
    checkpoint_publish::{self, LogPosition},
    data_record::DataRecord,
    index::Checkpoint,
    mapping_page::{MappingPageRecord, PageExpectation},
    reservation::ensure_space,
    storage,
    tail::{self, LogRecord},
};
use crate::{
    AlignedBlock, BLOCK_SIZE,
    accounted_io::BackingIo,
    invalid_data,
    mapping_cache::{
        Access, Acquisition, CacheError, ENTRIES_PER_PAGE, Lookups, MappingCache, MappingEntries,
    },
    read_block_exact,
};
use std::{fs::File, io, ops::Range};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RecoveryStats {
    pub(crate) newly_mapped_blocks: u64,
    pub(crate) tail_passes: u64,
    pub(crate) records_scanned: u64,
    pub(crate) page_reads: u64,
    pub(crate) snapshot_writes: u64,
    pub(crate) index_bytes: u64,
    pub(crate) lookups: Lookups,
}

pub(crate) struct Recovered {
    pub(crate) cache: MappingCache,
    pub(crate) tail: LogPosition,
    pub(crate) stats: RecoveryStats,
}

struct Recovery<'a> {
    backing_io: &'a mut BackingIo,
    backing: &'a File,
    active: &'a mut Checkpoint,
    candidate: Checkpoint,
    end: LogPosition,
    stats: RecoveryStats,
}

pub(crate) fn recover(
    backing_io: &mut BackingIo,
    backing: &File,
    active: &mut Checkpoint,
    budget: Option<u64>,
) -> io::Result<Recovered> {
    let logical_bytes = u64::from(active.descriptor.volume_blocks) * BLOCK_SIZE as u64;
    let mut cache = MappingCache::new(logical_bytes, budget).map_err(cache_error)?;
    let candidate = active.try_clone()?;
    let end = checkpoint_position(active);
    let mut recovery = Recovery {
        backing_io,
        backing,
        active,
        candidate,
        end,
        stats: RecoveryStats::default(),
    };
    let touched = recovery.scan()?;
    ensure_space(
        recovery.end.boundary,
        recovery.active.descriptor.backing_blocks,
        touched,
    )?;
    let capacity = cache.snapshot().sizing.capacity;
    if touched <= capacity as u64 {
        if touched != 0 {
            recovery.replay_window(
                &mut cache,
                0..recovery.candidate.mapping_page_block.len() as u32,
            )?;
        }
        recovery.count_lookups(&cache);
        return Ok(Recovered {
            cache,
            tail: recovery.end,
            stats: recovery.stats,
        });
    }
    recovery.publish_windows(cache, budget)
}

impl Recovery<'_> {
    fn publish_windows(
        mut self,
        mut cache: MappingCache,
        budget: Option<u64>,
    ) -> io::Result<Recovered> {
        self.prepare_publication()?;
        let capacity = cache.snapshot().sizing.capacity;
        let logical_bytes = u64::from(self.active.descriptor.volume_blocks) * BLOCK_SIZE as u64;
        let mut start = 0;
        while let Some(window) = self.next_window(start, capacity) {
            start = window.end;
            self.replay_window(&mut cache, window)?;
            checkpoint_publish::append_snapshots(
                self.backing_io,
                self.backing,
                &cache,
                &mut self.candidate,
            )?;
            self.stats.snapshot_writes += cache.dirty_pages().count() as u64;
            self.count_lookups(&cache);
            if self.next_window(start, capacity).is_some() {
                // The fixed replay source and staged snapshots survive this window's cache.
                drop(cache);
                cache = MappingCache::new(logical_bytes, budget).map_err(cache_error)?;
            }
        }
        storage::sync(self.backing_io, self.backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("recovery-snapshots-fsync-complete")?;
        checkpoint_publish::persist_root(self.backing_io, self.backing, &mut self.candidate)?;
        self.stats.index_bytes =
            u64::from(self.candidate.descriptor.geometry()?.index_blocks) * BLOCK_SIZE as u64;
        let tail = checkpoint_position(&self.candidate);
        *self.active = self.candidate;
        cache.checkpoint_published();
        Ok(Recovered {
            cache,
            tail,
            stats: self.stats,
        })
    }

    fn scan(&mut self) -> io::Result<u64> {
        self.stats.tail_passes += 1;
        let mut touched = 0;
        while let Some(record) = tail::read_next(
            self.backing_io,
            self.backing,
            self.active.descriptor,
            self.end,
        )? {
            self.end = record.position();
            self.stats.records_scanned += 1;
            let LogRecord::Data(data) = record else {
                continue;
            };
            for lba in data.lba_ids.iter().take(data.header.payload_count()?) {
                let page_id = *lba as usize / ENTRIES_PER_PAGE;
                let pointer = &mut self.candidate.mapping_page_block[page_id];
                // Valid physical pointers begin at log_start >= 4; one is a planning marker.
                if *pointer != 1 {
                    *pointer = 1;
                    touched += 1;
                }
            }
        }
        Ok(touched)
    }

    fn prepare_publication(&mut self) -> io::Result<()> {
        let descriptor = &mut self.candidate.descriptor;
        descriptor.generation = descriptor.next_generation()?;
        descriptor.slot = descriptor.slot.other();
        descriptor.checkpoint_lsn = self.end.lsn;
        descriptor.last_record_boundary = self.end.boundary;
        Ok(())
    }

    fn next_window(&self, start: u32, capacity: usize) -> Option<Range<u32>> {
        let mut pages = self
            .candidate
            .mapping_page_block
            .iter()
            .enumerate()
            .skip(start as usize)
            .filter(|(_, pointer)| **pointer == 1)
            .map(|(id, _)| id as u32);
        let first = pages.next()?;
        let last = pages.take(capacity - 1).last().unwrap_or(first);
        Some(first..last + 1)
    }

    fn replay_window(&mut self, cache: &mut MappingCache, window: Range<u32>) -> io::Result<()> {
        self.stats.tail_passes += 1;
        let mut position = checkpoint_position(self.active);
        while position != self.end {
            let record = tail::read_next(
                self.backing_io,
                self.backing,
                self.active.descriptor,
                position,
            )?
            .ok_or_else(|| invalid_data("recovery tail changed after scan"))?;
            position = record.position();
            if position.boundary > self.end.boundary || position.lsn > self.end.lsn {
                return Err(invalid_data("recovery passed its fixed replay boundary"));
            }
            self.stats.records_scanned += 1;
            if let LogRecord::Data(data) = record {
                self.apply(cache, &data, &window)?;
            }
        }
        Ok(())
    }

    fn apply(
        &mut self,
        cache: &mut MappingCache,
        data: &DataRecord,
        window: &Range<u32>,
    ) -> io::Result<()> {
        for index in 0..data.header.payload_count()? {
            let lba = data.lba_ids[index];
            let page_id = lba / ENTRIES_PER_PAGE as u32;
            if !window.contains(&page_id) {
                continue;
            }
            let mut page = match cache
                .acquire(page_id, Access::Recovery)
                .map_err(cache_error)?
            {
                Acquisition::Resident(page) => page,
                Acquisition::Vacant(vacant) => vacant
                    .install(self.read_page(page_id)?)
                    .map_err(cache_error)?,
                Acquisition::CheckpointRequired(_) => {
                    return Err(invalid_data("recovery window exceeds cache capacity"));
                }
            };
            let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
            let newly_mapped = page.entries().physical_addr[entry] == 0;
            page.update(
                entry,
                data.header.previous_boundary + 1 + index as u32,
                data.checksums[index],
            )
            .map_err(cache_error)?;
            self.stats.newly_mapped_blocks += u64::from(newly_mapped);
        }
        Ok(())
    }

    fn read_page(&mut self, page_id: u32) -> io::Result<MappingEntries> {
        let descriptor = self.active.descriptor;
        let boundary = self.active.mapping_page_block[page_id as usize];
        if boundary == 0 {
            return Ok(MappingEntries::zeroed());
        }
        let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
        read_block_exact(self.backing_io, self.backing, boundary, &mut bytes)?;
        self.backing_io.mapping_io.page_reads += 1;
        self.stats.page_reads += 1;
        Ok(MappingPageRecord::decode(
            &bytes.0,
            &PageExpectation {
                geometry: descriptor.geometry()?,
                volume_id: descriptor.volume_id,
                page_id,
                boundary,
                maximum_lsn: descriptor.checkpoint_lsn,
            },
        )?
        .entries)
    }

    fn count_lookups(&mut self, cache: &MappingCache) {
        let lookups = cache.snapshot().counters.recovery;
        self.stats.lookups.hits += lookups.hits;
        self.stats.lookups.misses += lookups.misses;
    }
}

fn checkpoint_position(checkpoint: &Checkpoint) -> LogPosition {
    LogPosition {
        lsn: checkpoint.descriptor.checkpoint_lsn,
        boundary: checkpoint.descriptor.last_record_boundary,
    }
}

fn cache_error(error: CacheError) -> io::Error {
    io::Error::other(format!("mapping cache during recovery: {error:?}"))
}
