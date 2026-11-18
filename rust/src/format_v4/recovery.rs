//! Two-head replay in global LSN order, bounded cache windows and stale-tail sanitation.
use super::{
    Geometry, HeadId, RecordKind,
    data_record::DataRecord,
    heads::{Head, plan_append},
    log::Log,
    mapping_page::{MappingPageRecord, PageExpectation},
    small_records::{BackingGrowth, LbaDiscard, SegmentLink},
    storage,
    tail::{Probe, probe_next},
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
    /// Net change in mapped logical blocks applied by the tail.
    pub(crate) mapped_delta: i64,
    pub(crate) tail_passes: u64,
    pub(crate) records_scanned: u64,
    pub(crate) page_reads: u64,
    pub(crate) snapshot_writes: u64,
    pub(crate) lookups: Lookups,
    pub(crate) touched_pages: u64,
}

pub(crate) struct Recovered {
    pub(crate) cache: MappingCache,
    pub(crate) stats: RecoveryStats,
}

/// Walks both head chains, yielding records in global LSN order.
struct Walker {
    heads: [Head; 2],
    expected: u64,
    stop_lsn: u64,
    probes: [Option<Probe>; 2],
}

impl Walker {
    fn new(heads: [Head; 2], first_lsn: u64, stop_lsn: u64) -> Self {
        Self {
            heads,
            expected: first_lsn,
            stop_lsn,
            probes: [None, None],
        }
    }

    fn next(
        &mut self,
        io: &mut BackingIo,
        backing: &File,
        geometry: Geometry,
        volume_id: u64,
    ) -> io::Result<Option<(HeadId, Probe)>> {
        if self.expected > self.stop_lsn {
            return Ok(None);
        }
        for id in HeadId::ALL {
            if self.probes[id.index()].is_none() {
                self.probes[id.index()] = probe_next(
                    io,
                    backing,
                    geometry,
                    volume_id,
                    self.heads[id.index()],
                    self.expected,
                )?;
            }
        }
        let Some(id) = HeadId::ALL.into_iter().find(|id| {
            self.probes[id.index()]
                .as_ref()
                .is_some_and(|probe| probe.header().lsn == self.expected)
        }) else {
            return Ok(None);
        };
        let probe = self.probes[id.index()].take().expect("probe present");
        self.heads[id.index()].advance(probe.header());
        self.expected += 1;
        Ok(Some((id, probe)))
    }

    /// The chain continues in `segment` after a link; drop any probe made at the old position.
    fn follow_link(&mut self, id: HeadId, segment: u32, geometry: Geometry) -> io::Result<()> {
        self.heads[id.index()].open(segment, geometry)?;
        self.probes[id.index()] = None;
        Ok(())
    }
}

struct Recovery<'a> {
    io: &'a mut BackingIo,
    backing: &'a File,
    log: &'a mut Log,
    actual_backing_blocks: u64,
    start_heads: [Head; 2],
    first_lsn: u64,
    /// The last LSN of the valid prefix; recovery output appends after it.
    end_lsn: u64,
    touched: Box<[u64]>,
    stats: RecoveryStats,
}

/// Recovers the tail after `load`. `live_blocks` receives the tail's net mapping change.
pub(crate) fn recover(
    io: &mut BackingIo,
    backing: &File,
    log: &mut Log,
    actual_backing_blocks: u64,
    budget: Option<u64>,
    live_blocks: &mut u64,
) -> io::Result<Recovered> {
    let logical_bytes = u64::from(log.geometry.volume_blocks) * BLOCK_SIZE as u64;
    let mut cache = MappingCache::new(logical_bytes, budget).map_err(cache_error)?;
    let page_words = (log.geometry.page_count as usize).div_ceil(u64::BITS as usize);
    let start_heads = log.heads;
    let first_lsn = log.next_lsn;
    let mut recovery = Recovery {
        io,
        backing,
        log,
        actual_backing_blocks,
        start_heads,
        first_lsn,
        end_lsn: first_lsn - 1,
        touched: super::segments::filled(page_words, 0)?,
        stats: RecoveryStats::default(),
    };
    let touched = recovery.scan()?;
    recovery.stats.touched_pages = touched;
    let capacity = cache.snapshot().sizing.capacity as u64;
    if touched <= capacity {
        if touched != 0 {
            recovery.replay_window(&mut cache, 0..recovery.log.geometry.page_count)?;
        }
        recovery.count_lookups(&cache);
        *live_blocks = apply_delta(*live_blocks, recovery.stats.mapped_delta)?;
        let stats = recovery.stats;
        recovery.sanitize()?;
        return Ok(Recovered { cache, stats });
    }
    recovery.reserve_output(touched, capacity)?;
    let result = recovery.publish_windows(&mut cache, capacity)?;
    *live_blocks = apply_delta(*live_blocks, result.mapped_delta)?;
    recovery.sanitize()?;
    Ok(Recovered {
        cache,
        stats: result,
    })
}

fn apply_delta(live: u64, delta: i64) -> io::Result<u64> {
    live.checked_add_signed(delta)
        .ok_or_else(|| invalid_data("live block count underflow"))
}

impl Recovery<'_> {
    fn mark_touched(&mut self, page: u32) -> bool {
        let word = &mut self.touched[page as usize / 64];
        let bit = 1_u64 << (page % 64);
        let new = *word & bit == 0;
        *word |= bit;
        new
    }

    fn is_touched(&self, page: u32) -> bool {
        self.touched[page as usize / 64] & (1_u64 << (page % 64)) != 0
    }

    /// Follows both chains once, applying structural records and noting touched pages.
    fn scan(&mut self) -> io::Result<u64> {
        self.stats.tail_passes += 1;
        let mut touched = 0;
        let mut walker = Walker::new(self.log.heads, self.first_lsn, u64::MAX);
        while let Some((id, probe)) =
            walker.next(self.io, self.backing, self.log.geometry, self.log.volume_id)?
        {
            self.stats.records_scanned += 1;
            let header = probe.header();
            let segment = self.log.head(id).segment;
            match probe {
                Probe::Data(record) => {
                    let count = header.payload_count(self.log.geometry)?;
                    for lba in record.lba_ids.iter().take(count) {
                        touched += u64::from(self.mark_touched(lba / ENTRIES_PER_PAGE as u32));
                    }
                    self.log.segments.record_used(segment, count as u32 + 1);
                }
                Probe::Metadata { block, .. } => {
                    self.log.segments.record_used(segment, 1);
                    match header.kind {
                        RecordKind::SegmentLink => {
                            let link = SegmentLink::decode(
                                &block.0,
                                self.log.geometry,
                                self.log.volume_id,
                            )?;
                            if link.head_id != id {
                                return Err(invalid_data("segment link names the other head"));
                            }
                            self.log.segments.replay_open(link.next_segment, id)?;
                            self.log.segments.seal(segment, id, header.lsn)?;
                            walker.follow_link(id, link.next_segment, self.log.geometry)?;
                        }
                        RecordKind::BackingGrowth => {
                            let growth = BackingGrowth::decode(
                                &block.0,
                                self.log.geometry,
                                self.log.volume_id,
                            )?;
                            if growth.new_backing_blocks > self.actual_backing_blocks {
                                return Err(invalid_data(
                                    "backing is shorter than a replayed growth record",
                                ));
                            }
                            let geometry =
                                self.log.geometry.with_backing(growth.new_backing_blocks)?;
                            self.log.segments.grow(geometry)?;
                            self.log.geometry = geometry;
                        }
                        RecordKind::LbaDiscard => {
                            let discard = LbaDiscard::decode(
                                &block.0,
                                self.log.geometry,
                                self.log.volume_id,
                            )?;
                            for (start, count) in discard.ranges() {
                                let first = start / ENTRIES_PER_PAGE as u32;
                                let last = (start + count - 1) / ENTRIES_PER_PAGE as u32;
                                for page in first..=last {
                                    touched += u64::from(self.mark_touched(page));
                                }
                            }
                        }
                        RecordKind::MappingPage
                        | RecordKind::MappingIndexPage
                        | RecordKind::SegmentTablePage => {}
                        RecordKind::Data => unreachable!("data records are probed separately"),
                    }
                }
            }
            self.log.heads = walker.heads;
            self.log.blocks_since_checkpoint +=
                u64::from(header.payload_count(self.log.geometry)? as u32) + 1;
        }
        self.log.next_lsn = walker.expected;
        self.end_lsn = walker.expected - 1;
        self.log.check_heads()?;
        Ok(touched)
    }

    /// Recovery output must fit without touching the reserve's guarantee for later progress.
    fn reserve_output(&self, touched: u64, capacity: u64) -> io::Result<()> {
        let geometry = self.log.geometry;
        let metadata = touched
            + u64::from(geometry.direct_pages)
            + u64::from(geometry.indirect_pages)
            + u64::from(geometry.table_pages);
        let room = self.log.head(HeadId::User).room(geometry);
        let plan = plan_append(geometry, room, 0, metadata);
        if plan.new_segments > self.log.segments.allocatable_count() {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        // Each window's snapshots must be appendable before the window's cache is dropped.
        assert!(capacity >= 1);
        Ok(())
    }

    fn publish_windows(
        &mut self,
        cache: &mut MappingCache,
        capacity: u64,
    ) -> io::Result<RecoveryStats> {
        let mut start = 0;
        while let Some(window) = self.next_window(start, capacity) {
            start = window.end;
            self.replay_window(cache, window)?;
            self.log.append_snapshots(self.io, self.backing, cache)?;
            self.stats.snapshot_writes += cache.dirty_pages().count() as u64;
            self.count_lookups(cache);
            cache.reset();
        }
        storage::sync(self.io, self.backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("recovery-snapshots-fsync-complete")?;
        self.log
            .complete_publication(self.io, self.backing, cache)?;
        Ok(self.stats)
    }

    fn next_window(&self, start: u32, capacity: u64) -> Option<Range<u32>> {
        let mut pages = (start..self.log.geometry.page_count).filter(|page| self.is_touched(*page));
        let first = pages.next()?;
        let last = pages.take(capacity as usize - 1).last().unwrap_or(first);
        Some(first..last + 1)
    }

    /// Replays the fixed tail, applying mappings for pages inside `window`.
    fn replay_window(&mut self, cache: &mut MappingCache, window: Range<u32>) -> io::Result<()> {
        self.stats.tail_passes += 1;
        let mut walker = Walker::new(self.start_heads, self.first_lsn, self.end_lsn);
        while let Some((id, probe)) =
            walker.next(self.io, self.backing, self.log.geometry, self.log.volume_id)?
        {
            self.stats.records_scanned += 1;
            match probe {
                Probe::Data(record) => self.apply_data(cache, &record, &window)?,
                Probe::Metadata { header, block } => match header.kind {
                    RecordKind::SegmentLink => {
                        let link =
                            SegmentLink::decode(&block.0, self.log.geometry, self.log.volume_id)?;
                        walker.follow_link(id, link.next_segment, self.log.geometry)?;
                    }
                    RecordKind::LbaDiscard => {
                        let discard =
                            LbaDiscard::decode(&block.0, self.log.geometry, self.log.volume_id)?;
                        self.apply_discard(cache, &discard, &window)?;
                    }
                    _ => {}
                },
            }
        }
        if walker.expected != self.end_lsn + 1 {
            return Err(invalid_data("recovery tail changed after scan"));
        }
        Ok(())
    }

    fn acquire<'c>(
        &mut self,
        cache: &'c mut MappingCache,
        page_id: u32,
    ) -> io::Result<crate::mapping_cache::CachedPage<'c>> {
        match cache
            .acquire(page_id, Access::Recovery)
            .map_err(cache_error)?
        {
            Acquisition::Resident(page) => Ok(page),
            Acquisition::Vacant(vacant) => {
                let entries = self.read_page(page_id)?;
                vacant.install(entries).map_err(cache_error)
            }
            Acquisition::CheckpointRequired(_) => {
                Err(invalid_data("recovery window exceeds cache capacity"))
            }
        }
    }

    fn apply_data(
        &mut self,
        cache: &mut MappingCache,
        data: &DataRecord,
        window: &Range<u32>,
    ) -> io::Result<()> {
        let count = data.header.payload_count(self.log.geometry)?;
        for index in 0..count {
            let lba = data.lba_ids[index];
            let page_id = lba / ENTRIES_PER_PAGE as u32;
            if !window.contains(&page_id) {
                continue;
            }
            let physical = data.payload_block(self.log.geometry, index)?;
            let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
            let mut page = self.acquire(cache, page_id)?;
            let old = page.entries().physical_addr[entry];
            page.update(entry, physical, data.checksums[index])
                .map_err(cache_error)?;
            if old != 0 {
                self.log.segments.remove_live(old)?;
            } else {
                self.stats.mapped_delta += 1;
            }
            self.log.segments.add_live(physical)?;
        }
        Ok(())
    }

    fn apply_discard(
        &mut self,
        cache: &mut MappingCache,
        discard: &LbaDiscard,
        window: &Range<u32>,
    ) -> io::Result<()> {
        for (start, count) in discard.ranges() {
            for lba in start..start + count {
                let page_id = lba / ENTRIES_PER_PAGE as u32;
                if !window.contains(&page_id) {
                    continue;
                }
                let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
                let mut page = self.acquire(cache, page_id)?;
                let old = page.entries().physical_addr[entry];
                if old == 0 {
                    continue;
                }
                page.update(entry, 0, 0).map_err(cache_error)?;
                self.log.segments.remove_live(old)?;
                self.stats.mapped_delta -= 1;
            }
        }
        Ok(())
    }

    fn read_page(&mut self, page_id: u32) -> io::Result<MappingEntries> {
        let boundary = self.log.index.snapshot_block(page_id);
        if boundary == 0 {
            return Ok(MappingEntries::zeroed());
        }
        let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
        read_block_exact(self.io, self.backing, boundary, &mut bytes)?;
        self.io.mapping_io.page_reads += 1;
        self.stats.page_reads += 1;
        Ok(MappingPageRecord::decode(
            &bytes.0,
            &PageExpectation {
                geometry: self.log.geometry,
                volume_id: self.log.volume_id,
                page_id,
                boundary,
                maximum_lsn: self.log.checkpoint_lsn(),
            },
        )?
        .entries)
    }

    fn count_lookups(&mut self, cache: &MappingCache) {
        let lookups = cache.snapshot().counters.recovery;
        self.stats.lookups.hits += lookups.hits;
        self.stats.lookups.misses += lookups.misses;
    }

    /// Zeroes the rest of each head's open segment and persists it, so no record left by an
    /// interrupted run can chain onto a later append. A 339-block window is not enough once a
    /// torn maximal record can hide a complete successor beyond it inside the same segment.
    fn sanitize(&mut self) -> io::Result<()> {
        for id in HeadId::ALL {
            let head = self.log.head(id);
            let end = self.log.geometry.segment_end(head.segment)?;
            let start = u64::from(head.next_block);
            storage::zero_blocks(self.io, self.backing, start, end.saturating_sub(start))?;
        }
        storage::sync(self.io, self.backing)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::STALE_TAIL_FSYNC_COMPLETE_FAILPOINT)?;
        Ok(())
    }
}

fn cache_error(error: CacheError) -> io::Error {
    io::Error::other(format!("mapping cache during recovery: {error:?}"))
}
