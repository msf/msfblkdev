//! The writer's side of reclamation: apply proposals, run the freeing barrier, discard.
use super::*;
use crate::format_v4::{
    RecordHeader, RecordKind,
    data_record::DataRecord,
    mapping_page::{MappingPageRecord, PageExpectation},
    reclaim::{
        Entry, MetadataKind, ProposalBuffer, ReclaimStats, View,
        scanner::{ScanContext, ScanCursor, Scanner},
        select::{Selection, SelectionMode, select},
    },
    reservation::ensure_user_space,
};
use crate::{
    MAXIMUM_PAYLOAD_BLOCKS,
    accounted_io::{submit_exact, submit_exact_unaccounted},
};
use io_uring::{opcode, types};
use std::collections::VecDeque;
use std::os::fd::AsRawFd;

/// What one maintenance step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceStep {
    /// A scanned window was applied.
    Applied,
    /// A victim window was read.
    Scanned,
    /// A checkpoint advanced the freeing barrier; `freed` segments became allocatable.
    Published { freed: u32 },
    /// Nothing to do: no pending release, no proposals and no eligible victim.
    Idle,
}

/// How free segments are returned to the backing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackingDiscard {
    Off,
    /// `fallocate(PUNCH_HOLE | KEEP_SIZE)` on regular files after durable release.
    #[default]
    HolePunch,
}

pub(crate) struct Reclaim {
    scanner: Scanner,
    view: View,
    /// The checkpoint LSN the view was copied at; a newer root refreshes it before a scan.
    view_checkpoint_lsn: Option<u64>,
    pool: Vec<ProposalBuffer>,
    queue: VecDeque<ProposalBuffer>,
    cursor: Option<ScanCursor>,
    accepted: Box<[u32]>,
    pub(crate) stats: ReclaimStats,
    discard: BackingDiscard,
    discard_disabled: bool,
}

impl Reclaim {
    pub(crate) fn new(
        geometry: Geometry,
        shape: CleanerShape,
        discard: BackingDiscard,
    ) -> io::Result<Self> {
        let mut pool = Vec::new();
        pool.try_reserve_exact(shape.proposal_buffers as usize)
            .map_err(|_| io::Error::other("proposal pool allocation failed"))?;
        for _ in 0..shape.proposal_buffers {
            pool.push(ProposalBuffer::new(shape)?);
        }
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(shape.proposal_buffers as usize)
            .map_err(|_| io::Error::other("proposal queue allocation failed"))?;
        Ok(Self {
            scanner: Scanner::new()?,
            view: View::new(geometry)?,
            view_checkpoint_lsn: None,
            pool,
            queue,
            cursor: None,
            accepted: crate::format_v4::segments::filled(shape.window_blocks as usize, 0)?,
            stats: ReclaimStats::default(),
            discard,
            discard_disabled: false,
        })
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.view.allocated_bytes()
            + self
                .pool
                .iter()
                .chain(self.queue.iter())
                .map(|buffer| buffer.allocated_bytes())
                .sum::<usize>()
            + std::mem::size_of_val(self.accepted.as_ref())
            + self.scanner.allocated_bytes()
    }
}

impl Volume {
    /// Reclamation statistics since open.
    pub fn reclaim_stats(&self) -> ReclaimStats {
        self.reclaim.stats
    }

    /// Bytes appended per head and record class since open.
    pub fn log_writes(&self) -> crate::LogWrites {
        self.log.writes
    }

    /// Runs one bounded maintenance step. Under `Pressure` the writer prefers the emptiest
    /// victim; `Idle` prefers old, mostly dead segments. Running out of space during
    /// maintenance is reported as `ENOSPC` without failing the volume.
    pub fn maintain(&mut self, mode: SelectionMode) -> io::Result<MaintenanceStep> {
        self.check_operational()?;
        match self.maintain_step(mode) {
            Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => Err(error),
            result => self.latch(result),
        }
    }

    /// Whether `copy_blocks` on the Relocation head and `publications` checkpoints on the
    /// User head fit the room of each head plus the allocatable segments.
    fn maintenance_fits(&self, copy_blocks: u64, publications: u64) -> bool {
        let geometry = self.log.geometry;
        let writable = u64::from(geometry.blocks_per_segment - 1);
        let room_user = u64::from(self.log.head(HeadId::User).room(geometry));
        let room_relocation = u64::from(self.log.head(HeadId::Relocation).room(geometry));
        let copy_segments = copy_blocks
            .saturating_sub(room_relocation)
            .div_ceil(writable);
        let publication_blocks = publications * self.publication_blocks();
        let publication_segments = publication_blocks
            .saturating_sub(room_user)
            .div_ceil(writable);
        copy_segments + publication_segments <= u64::from(self.log.segments.allocatable_count())
    }

    /// Blocks user admission must leave alone while a victim waits for its second publication.
    pub(crate) fn maintenance_debt(&self) -> u64 {
        if self.log.segments.pending_free_count() > 0 {
            self.publication_blocks()
        } else {
            0
        }
    }

    /// Blocks one publication appends now: dirty snapshots, dirty index and table pages, and
    /// the links a crossing on each head could need.
    fn publication_blocks(&self) -> u64 {
        self.mapping.cache.dirty_pages().count() as u64
            + self.log.index.dirty_page_count()
            + u64::from(self.log.segments.dirty_table_page_count())
            + 2
    }

    /// Whether relocating `victim` and then publishing twice fits the space left, so that a
    /// started victim can always be released (the progress contract).
    fn can_start_victim(&self, victim: u32) -> bool {
        let live = u64::from(self.log.segments.live_blocks(victim));
        let copies = live + live.div_ceil(MAXIMUM_PAYLOAD_BLOCKS as u64) + 1;
        self.maintenance_fits(copies, 2)
    }

    /// Whether one publication fits the space left. A publication that cannot fit fails with
    /// `ENOSPC` instead of latching the volume.
    fn publication_fits(&self) -> bool {
        self.maintenance_fits(0, 1)
    }

    fn maintain_step(&mut self, mode: SelectionMode) -> io::Result<MaintenanceStep> {
        if let Some(buffer) = self.reclaim.queue.pop_front() {
            let result = self.apply_buffer(&buffer);
            self.reclaim.pool.push(buffer);
            result?;
            return Ok(MaintenanceStep::Applied);
        }
        if self.reclaim.cursor.is_some_and(|cursor| !cursor.done) {
            return Ok(if self.scan_next_window()? {
                MaintenanceStep::Scanned
            } else {
                // Every window buffer is held elsewhere; nothing can be read until one returns.
                MaintenanceStep::Idle
            });
        }
        if self.log.segments.pending_free_count() > 0 {
            return self.barrier_publication();
        }
        match select(&self.log, mode, None) {
            Selection::Victim(victim) if !self.can_start_victim(victim) => {
                // Copies would eat into the space the freeing publications need.
                Ok(MaintenanceStep::Idle)
            }
            Selection::Victim(victim) => {
                self.refresh_view();
                let cursor = ScanCursor::start(
                    self.log.geometry,
                    victim,
                    self.log.segments.seal_lsn(victim),
                )?;
                self.reclaim.cursor = Some(cursor);
                self.reclaim.stats.victims_selected += 1;
                Ok(if self.scan_next_window()? {
                    MaintenanceStep::Scanned
                } else {
                    MaintenanceStep::Idle
                })
            }
            // A recently sealed segment becomes eligible once both roots pass its seal LSN.
            Selection::AwaitPublication => self.barrier_publication(),
            Selection::Nothing => Ok(MaintenanceStep::Idle),
        }
    }

    /// Takes the next scanned window out of the queue without applying it (tests only).
    #[cfg(test)]
    pub(crate) fn take_queued(&mut self) -> Option<ProposalBuffer> {
        self.reclaim.queue.pop_front()
    }

    /// Publishes so that pending victims move one barrier step and refreshes the view.
    fn barrier_publication(&mut self) -> io::Result<MaintenanceStep> {
        if !self.publication_fits() {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        // Segments pending in the active root become free when this descriptor is durable.
        let freed = self.log.segments.published_free_transitions();
        let published = self
            .mapping
            .publish(&mut self.backing_io, &self.backing, &mut self.log)?;
        if !published {
            // Nothing was dirty: append a fresh table page so the publication advances the log.
            self.log.segments.mark_table_dirty(0);
            self.mapping
                .publish(&mut self.backing_io, &self.backing, &mut self.log)?;
        }
        self.reclaim.stats.barrier_publications += 1;
        self.reclaim.stats.segments_freed += u64::from(freed);
        self.discard_freed_segments()?;
        Ok(MaintenanceStep::Published { freed })
    }

    /// Copies the resident index into the scan view when a newer root exists. An eligible
    /// victim sealed at or before both roots' LSNs has every write into it reflected in the
    /// active root, so a payload this view calls dead can never become live again; a payload
    /// it calls live may have died since, which the apply check catches.
    fn refresh_view(&mut self) {
        let checkpoint_lsn = self.log.checkpoint_lsn();
        if self.reclaim.view_checkpoint_lsn == Some(checkpoint_lsn) {
            return;
        }
        self.log
            .index
            .snapshot_blocks()
            .iter()
            .zip(self.reclaim.view.snapshot_block.iter_mut())
            .for_each(|(block, target)| *target = *block);
        self.reclaim.view.maximum_lsn = self.log.next_lsn - 1;
        self.reclaim.view_checkpoint_lsn = Some(checkpoint_lsn);
    }

    /// Reads the next window of the current victim; false when no window buffer is free.
    fn scan_next_window(&mut self) -> io::Result<bool> {
        let mut cursor = self
            .reclaim
            .cursor
            .take()
            .expect("a victim is being scanned");
        let Some(mut buffer) = self.reclaim.pool.pop() else {
            self.reclaim.cursor = Some(cursor);
            return Ok(false);
        };
        let context = ScanContext {
            backing: &self.backing,
            geometry: self.log.geometry,
            volume_id: self.log.volume_id,
            view: &self.reclaim.view,
        };
        let report = self
            .reclaim
            .scanner
            .scan_window(&context, &mut cursor, &mut buffer)?;
        self.reclaim.stats.windows_scanned += 1;
        self.reclaim.stats.scan_blocks_read += report.blocks_read;
        self.reclaim.stats.payloads_proposed += report.payloads_live;
        self.reclaim.queue.push_back(buffer);
        self.reclaim.cursor = Some(cursor);
        Ok(true)
    }

    /// Validates one scanned window against live state and appends the accepted copies.
    pub(crate) fn apply_buffer(&mut self, buffer: &ProposalBuffer) -> io::Result<()> {
        let victim = buffer.victim;
        let segments = &self.log.segments;
        if segments.state(victim) != crate::format_v4::segment_table::SegmentState::Sealed
            || segments.seal_lsn(victim) != buffer.seal_lsn
        {
            self.reclaim.stats.proposals_rejected_source += 1;
            return Ok(());
        }
        self.copy_payloads(buffer)?;
        for entry in buffer.entries() {
            match *entry {
                Entry::Payload { .. } => {}
                Entry::Snapshot {
                    slot,
                    page_id,
                    source_block,
                } => {
                    self.relocate_snapshot(buffer, slot, page_id, source_block)?;
                }
                Entry::Metadata {
                    kind,
                    id,
                    source_block,
                } => self.dirty_metadata(kind, id, source_block),
            }
        }
        if buffer.last_window {
            self.finish_victim(victim);
        }
        Ok(())
    }

    fn finish_victim(&mut self, victim: u32) {
        let segments = &mut self.log.segments;
        if segments.live_blocks(victim) == 0 && segments.metadata_blocks(victim) == 0 {
            segments
                .mark_pending_free(victim)
                .expect("sealed victim without references");
            self.reclaim.stats.victims_pending += 1;
        } else {
            // Something the scan could not move stays live; leave the victim alone until a
            // mapping change touches it again.
            segments.mark_stuck(victim);
        }
        self.reclaim.cursor = None;
    }

    /// Filters the window's payloads against current mappings and buffered writes, then packs
    /// the survivors into records on the Relocation head without copying their bytes.
    fn copy_payloads(&mut self, buffer: &ProposalBuffer) -> io::Result<()> {
        let mut accepted = 0;
        for (index, entry) in buffer.sorted_payloads().enumerate() {
            let Entry::Payload {
                lba, source_block, ..
            } = entry
            else {
                unreachable!("sorted payloads are payload entries");
            };
            if self.write_buffer.get(lba).is_some() || self.current_physical(lba)? != source_block {
                self.reclaim.stats.payloads_stale += 1;
                continue;
            }
            self.reclaim.accepted[accepted] = index as u32;
            accepted += 1;
        }
        let mut start = 0;
        // Bounded by the accepted payloads: each iteration writes some or opens a segment.
        while start < accepted {
            let room = self
                .log
                .head(HeadId::Relocation)
                .payload_room(self.log.geometry);
            if room == 0 {
                self.log
                    .advance_head(&mut self.backing_io, &self.backing, HeadId::Relocation)?;
                continue;
            }
            let end = (start + room.min(MAXIMUM_PAYLOAD_BLOCKS)).min(accepted);
            self.preflight_relocation(buffer, start..end)?;
            self.write_relocation_record(buffer, start..end)?;
            start = end;
        }
        Ok(())
    }

    fn current_physical(&mut self, lba: u32) -> io::Result<u32> {
        let page_id = lba / ENTRIES_PER_PAGE as u32;
        let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
        self.mapping.with_page(
            &mut self.backing_io,
            &self.backing,
            &mut self.log,
            page_id,
            CheckpointPolicy::Allow,
            |page| Ok(page.entries().physical_addr[entry]),
        )
    }

    fn accepted_payload(&self, buffer: &ProposalBuffer, index: usize) -> Entry {
        let order = self.reclaim.accepted[index] as usize;
        buffer.entries[buffer.payload_order[order] as usize]
    }

    /// Publishes first when the record's mapping pages and the dirty set would not fit.
    fn preflight_relocation(
        &mut self,
        buffer: &ProposalBuffer,
        range: std::ops::Range<usize>,
    ) -> io::Result<()> {
        let mut pages = 0;
        let mut last_page = None;
        for index in range.clone() {
            let Entry::Payload { lba, .. } = self.accepted_payload(buffer, index) else {
                unreachable!("accepted entries are payloads");
            };
            let page = lba / ENTRIES_PER_PAGE as u32;
            if last_page != Some(page) {
                pages += 1;
                last_page = Some(page);
            }
        }
        let first = match self.accepted_payload(buffer, range.start) {
            Entry::Payload { lba, .. } => lba / ENTRIES_PER_PAGE as u32,
            _ => unreachable!(),
        };
        let last = match self.accepted_payload(buffer, range.end - 1) {
            Entry::Payload { lba, .. } => lba / ENTRIES_PER_PAGE as u32,
            _ => unreachable!(),
        };
        let unrelated_dirty = self
            .mapping
            .cache
            .dirty_pages()
            .filter(|(page_id, _)| !(first..=last).contains(page_id))
            .count();
        let capacity = self.mapping.cache.snapshot().sizing.capacity;
        let record_bytes = (range.len() + 1) as u64 * BLOCK_SIZE as u64;
        if unrelated_dirty + pages > capacity
            || self.log.blocks_since_checkpoint * BLOCK_SIZE as u64 + record_bytes
                > self.checkpoint_after_bytes
        {
            self.mapping
                .publish(&mut self.backing_io, &self.backing, &mut self.log)?;
            self.mapping.pressure_checkpoints += 1;
        }
        Ok(())
    }

    fn write_relocation_record(
        &mut self,
        buffer: &ProposalBuffer,
        range: std::ops::Range<usize>,
    ) -> io::Result<()> {
        let count = range.len();
        let header = self.log.data_header(HeadId::Relocation, count);
        let start = header.start(self.log.geometry)?;
        let mut record = DataRecord::empty(header);
        let volume_id = self.log.volume_id;
        self.release_iovecs.0.clear();
        for (offset, index) in range.clone().enumerate() {
            let Entry::Payload { slot, lba, .. } = self.accepted_payload(buffer, index) else {
                unreachable!("accepted entries are payloads");
            };
            let bytes = buffer.payload(slot);
            record.lba_ids[offset] = lba;
            record.checksums[offset] =
                payload_checksum(volume_id, lba, start + offset as u32, bytes);
            self.release_iovecs.0.push(libc::iovec {
                iov_base: bytes.as_ptr() as *mut libc::c_void,
                iov_len: BLOCK_SIZE,
            });
        }
        record.encode(self.log.geometry, &mut self.footer_scratch.0)?;
        let iovecs = &mut self.release_iovecs.0;
        iovecs.push(libc::iovec {
            iov_base: self.footer_scratch.0.as_ptr() as *mut libc::c_void,
            iov_len: BLOCK_SIZE,
        });
        let write = opcode::Writev::new(
            types::Fd(self.backing.as_raw_fd()),
            iovecs.as_ptr(),
            iovecs.len() as u32,
        )
        .offset(u64::from(start) * BLOCK_SIZE as u64)
        .build();
        submit_exact(
            &mut self.backing_io,
            write,
            header.lsn,
            ((count + 1) * BLOCK_SIZE) as i32,
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("relocation-record-write-complete")?;
        self.log.data_committed(HeadId::Relocation, header)?;
        self.install_record_mappings(&record, count)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("relocation-mapping-installed")?;
        self.reclaim.stats.payloads_copied += count as u64;
        Ok(())
    }

    /// A current snapshot in the victim: dirty it when resident, else copy it as a record.
    fn relocate_snapshot(
        &mut self,
        buffer: &ProposalBuffer,
        slot: u32,
        page_id: u32,
        source_block: u32,
    ) -> io::Result<()> {
        if self.log.index.snapshot_block(page_id) != source_block {
            return Ok(());
        }
        match self.mapping.cache.residency(page_id) {
            Some(true) => Ok(()),
            Some(false) => {
                self.mapping.with_page(
                    &mut self.backing_io,
                    &self.backing,
                    &mut self.log,
                    page_id,
                    CheckpointPolicy::Allow,
                    |mut page| {
                        page.mark_dirty();
                        Ok(())
                    },
                )?;
                self.reclaim.stats.snapshots_dirtied += 1;
                Ok(())
            }
            None => {
                let decoded = MappingPageRecord::decode(
                    buffer.payload(slot),
                    &PageExpectation {
                        geometry: self.log.geometry,
                        volume_id: self.log.volume_id,
                        page_id,
                        boundary: source_block,
                        maximum_lsn: buffer.seal_lsn,
                    },
                )?;
                let entries = decoded.entries;
                let header = self.log.append_metadata(
                    &mut self.backing_io,
                    &self.backing,
                    HeadId::Relocation,
                    RecordKind::MappingPage,
                    |log, header: RecordHeader, bytes| {
                        MappingPageRecord {
                            header,
                            page_id,
                            entries: entries.clone(),
                        }
                        .encode(log.geometry, bytes)
                    },
                )?;
                self.log.snapshot_written(page_id, header.boundary)?;
                self.reclaim.stats.snapshots_copied += 1;
                Ok(())
            }
        }
    }

    fn dirty_metadata(&mut self, kind: MetadataKind, id: u32, source_block: u32) {
        match kind {
            MetadataKind::Direct if self.log.index.direct_block(id) == source_block => {
                self.log.index.mark_direct_dirty(id);
                self.reclaim.stats.index_pages_dirtied += 1;
            }
            MetadataKind::Indirect if self.log.index.indirect_block(id) == source_block => {
                self.log.index.mark_indirect_dirty(id);
                self.reclaim.stats.index_pages_dirtied += 1;
            }
            MetadataKind::Table if self.log.segments.table_block(id) == source_block => {
                self.log.segments.mark_table_dirty(id);
                self.reclaim.stats.table_pages_dirtied += 1;
            }
            _ => {}
        }
    }

    /// Punches holes for segments freed by the last barrier while excluding them from
    /// allocation. A backing without hole punching disables further discards.
    fn discard_freed_segments(&mut self) -> io::Result<()> {
        while let Some(segment) = self.log.segments.take_discard_due() {
            if self.reclaim.discard == BackingDiscard::Off || self.reclaim.discard_disabled {
                continue;
            }
            self.log.segments.begin_discard(segment)?;
            let start = u64::from(self.log.geometry.segment_start(segment)?) * BLOCK_SIZE as u64;
            let length = u64::from(self.log.geometry.blocks_per_segment) * BLOCK_SIZE as u64;
            let punch = opcode::Fallocate::new(types::Fd(self.backing.as_raw_fd()), length)
                .offset(start)
                .mode(libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE)
                .build();
            // Discards are neither reads nor writes: the accounting ledger does not count them.
            let result = submit_exact_unaccounted(&mut self.backing_io, punch, start, 0);
            #[cfg(any(test, feature = "test-failpoints"))]
            crate::pause_at_test_failpoint("backing-discard-complete")?;
            self.log.segments.finish_discard(segment);
            match result {
                Ok(()) => self.reclaim.stats.discards_issued += 1,
                Err(error) => {
                    // A free segment holds no live data, so a failed punch costs only space.
                    self.reclaim.stats.discards_failed += 1;
                    self.reclaim.discard_disabled = true;
                    if error.raw_os_error() != Some(libc::EOPNOTSUPP) {
                        crate::log_info!("event=backing_discard_disabled error={error}");
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs maintenance under pressure until `demanded_blocks` fit or nothing can free space.
    pub(crate) fn reclaim_for_space(&mut self, demanded_blocks: u64) -> io::Result<()> {
        // Bounded: every non-idle step scans, applies or publishes; Idle ends the loop.
        loop {
            if ensure_user_space(&self.log, self.reserve, demanded_blocks).is_ok() {
                return Ok(());
            }
            self.reclaim.stats.pressure_steps += 1;
            match self.maintain(SelectionMode::Pressure)? {
                MaintenanceStep::Idle => {
                    self.reclaim.stats.enospc_all_live += 1;
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
                _ => continue,
            }
        }
    }
}
