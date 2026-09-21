//! The writer's side of reclamation: hand windows to the cleaner thread, apply what it
//! proposes, run the freeing barrier and hand freed segments over for discard.
use super::*;
use crate::format_v4::{
    RecordHeader, RecordKind,
    data_record::DataRecord,
    mapping_page::{MappingPageRecord, PageExpectation},
    reclaim::{
        Entry, MetadataKind, ProposalBuffer, ReclaimStats, View,
        cleaner::{CleanerHandle, Command, DISCARD_SLOTS, Event},
        scanner::ScanCursor,
        select::{Selection, SelectionMode, select},
    },
    reservation::ensure_user_space,
};
use crate::{MAXIMUM_PAYLOAD_BLOCKS, accounted_io::submit_exact};
use io_uring::{opcode, types};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

/// How long a write under pressure waits for one cleaner window before it fails.
const PRESSURE_WAIT: Duration = Duration::from_secs(60);

/// What one maintenance step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceStep {
    /// A scanned window was applied.
    Applied,
    /// A victim window was handed to the cleaner for reading.
    Scanning,
    /// A checkpoint advanced the freeing barrier; `freed` segments became allocatable.
    Published { freed: u32 },
    /// The cleaner holds the only work there is; nothing changes until its event arrives.
    Waiting,
    /// Nothing to do: no pending release, no proposals and no eligible victim.
    Idle,
}

/// Where the current victim's scan stands on the writer's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanState {
    None,
    /// The next window can be requested with this cursor.
    Ready(ScanCursor),
    /// The cleaner holds the cursor and one window buffer.
    InFlight,
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
    cleaner: CleanerHandle,
    /// The view the writer refills and swaps into the cleaner; None while the swap is in flight.
    spare_view: Option<Box<View>>,
    /// The checkpoint LSN the cleaner's view was copied at; a newer root refreshes it.
    view_checkpoint_lsn: Option<u64>,
    pool: Vec<ProposalBuffer>,
    queue: VecDeque<ProposalBuffer>,
    scan: ScanState,
    accepted: Box<[u32]>,
    pub(crate) stats: ReclaimStats,
    discard: BackingDiscard,
    discard_disabled: bool,
    discards_in_flight: usize,
}

impl Reclaim {
    pub(crate) fn new(
        backing: &File,
        geometry: Geometry,
        volume_id: u64,
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
        let cleaner_view = Box::new(View::new(geometry)?);
        let spare_view = Box::new(View::new(geometry)?);
        Ok(Self {
            cleaner: CleanerHandle::spawn(backing, geometry, volume_id, cleaner_view)?,
            spare_view: Some(spare_view),
            view_checkpoint_lsn: None,
            pool,
            queue,
            scan: ScanState::None,
            accepted: crate::format_v4::segments::filled(shape.window_blocks as usize, 0)?,
            stats: ReclaimStats::default(),
            discard,
            discard_disabled: false,
            discards_in_flight: 0,
        })
    }

    /// Heap held by the writer's side and lent to the cleaner: both views, every window buffer
    /// and the acceptance scratch. The cleaner's own scratch is counted by `Scanner`.
    pub(crate) fn allocated_bytes(&self, geometry: Geometry) -> usize {
        2 * View::allocated_bytes_for(geometry)
            + self
                .pool
                .iter()
                .chain(self.queue.iter())
                .map(|buffer| buffer.allocated_bytes())
                .sum::<usize>()
            + std::mem::size_of_val(self.accepted.as_ref())
            + crate::format_v4::reclaim::scanner::Scanner::allocated_bytes()
    }
}

impl Volume {
    /// Reclamation statistics since open.
    pub fn reclaim_stats(&self) -> ReclaimStats {
        ReclaimStats {
            segments_released_without_scan: self.log.segments.emptied_count(),
            ..self.reclaim.stats
        }
    }

    /// Bytes appended per head and record class since open.
    pub fn log_writes(&self) -> crate::LogWrites {
        self.log.writes
    }

    /// Runs one bounded maintenance step without blocking. Under `Pressure` the writer
    /// prefers the emptiest victim; `Idle` prefers old, mostly dead segments. Running out of
    /// space during maintenance is reported as `ENOSPC` without failing the volume.
    pub fn maintain(&mut self, mode: SelectionMode) -> io::Result<MaintenanceStep> {
        self.check_operational()?;
        match self.maintain_step(mode) {
            Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => Err(error),
            result => self.latch(result),
        }
    }

    /// Waits up to `timeout` for the cleaner to report on outstanding work and applies the
    /// report to the writer's state. `Ok(false)` when nothing is outstanding or the wait timed
    /// out; the next `maintain` acts on what arrived.
    pub fn wait_for_cleaner(&mut self, timeout: Duration) -> io::Result<bool> {
        self.check_operational()?;
        if self.reclaim.cleaner.outstanding() == 0 {
            return Ok(false);
        }
        let waited = self.reclaim.cleaner.recv_timeout(timeout);
        match waited {
            Ok(Some(event)) => {
                let handled = self.handle_cleaner_event(event);
                self.latch(handled)?;
                let polled = self.poll_cleaner();
                self.latch(polled)?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => self.latch::<bool>(Err(error)),
        }
    }

    /// Commands the cleaner has not reported on yet: scans, view swaps and discards.
    pub fn cleaner_outstanding(&self) -> usize {
        self.reclaim.cleaner.outstanding()
    }

    /// A descriptor that becomes readable when the cleaner has reported; a request loop polls
    /// it and then calls `maintain`.
    pub fn cleaner_wakeup_fd(&self) -> RawFd {
        self.reclaim.cleaner.wakeup_fd()
    }

    /// Applies every event the cleaner has reported and clears its wake-up, without running
    /// a maintenance step. A request loop calls this when the wake-up descriptor fires but the
    /// scheduler says not to maintain yet; otherwise the descriptor stays readable and a
    /// re-armed poll completes at once.
    pub fn poll_cleaner_events(&mut self) -> io::Result<()> {
        self.check_operational()?;
        let polled = self.poll_cleaner();
        self.latch(polled)
    }

    /// Tells the cleaner the geometry it must scan and discard against after backing growth.
    pub(crate) fn update_cleaner_geometry(&mut self) -> io::Result<()> {
        self.reclaim
            .cleaner
            .send(Command::Geometry(self.log.geometry))
    }

    /// Whether `maintain` has work that does not depend on the cleaner reporting first.
    pub fn maintenance_ready(&self) -> bool {
        !self.reclaim.queue.is_empty()
            || matches!(self.reclaim.scan, ScanState::Ready(_)) && !self.reclaim.pool.is_empty()
            || self.log.segments.pending_free_count() > 0
    }

    /// Runs maintenance steps until the volume is idle, waits on the cleaner, or `slice`
    /// elapses, and returns the last step. Request loops call this at safe boundaries so
    /// that maintenance never holds the writer longer than one step past the slice.
    pub fn maintain_for(
        &mut self,
        mode: SelectionMode,
        slice: Duration,
    ) -> io::Result<MaintenanceStep> {
        let start = std::time::Instant::now();
        // Bounded by the slice: every step that continues the loop did bounded work.
        loop {
            let step = self.maintain(mode)?;
            match step {
                MaintenanceStep::Idle | MaintenanceStep::Waiting => return Ok(step),
                _ if start.elapsed() >= slice => return Ok(step),
                _ => {}
            }
        }
    }

    /// Makes the cleaner sleep before every window read (tests of delayed cleaner I/O).
    #[cfg(test)]
    pub(crate) fn delay_cleaner(&mut self, delay: Duration) -> io::Result<()> {
        self.reclaim.cleaner.send(Command::Delay(delay))
    }

    /// Makes the cleaner report its next window read as failed (tests of cleaner failure).
    #[cfg(test)]
    pub(crate) fn fail_next_cleaner_scan(&mut self) -> io::Result<()> {
        self.reclaim.cleaner.send(Command::FailNextScan)
    }

    /// Whether `copy_blocks` on the Relocation head and `publications` worst-case checkpoints
    /// fit the Relocation head's room plus the allocatable segments, leaving `keep_segments`
    /// untouched. Publications are counted at their reserve size because the copies themselves
    /// dirty mapping pages and enlarge the publications that must follow them.
    fn maintenance_fits(&self, copy_blocks: u64, publications: u64, keep_segments: u32) -> bool {
        let geometry = self.log.geometry;
        let writable = u64::from(geometry.blocks_per_segment - 1);
        let room_relocation = u64::from(self.log.head(HeadId::Relocation).room(geometry));
        let copy_segments = copy_blocks
            .saturating_sub(room_relocation)
            .div_ceil(writable);
        let publication_segments = publications * u64::from(self.reserve.publication_segments);
        copy_segments + publication_segments + u64::from(keep_segments)
            <= u64::from(self.log.segments.allocatable_count())
    }

    /// Whether one publication of the current dirty set fits the User head's room plus the
    /// allocatable segments.
    fn publication_fits(&self) -> bool {
        let geometry = self.log.geometry;
        let writable = u64::from(geometry.blocks_per_segment - 1);
        let room_user = u64::from(self.log.head(HeadId::User).room(geometry));
        let segments = self
            .publication_blocks()
            .saturating_sub(room_user)
            .div_ceil(writable);
        segments <= u64::from(self.log.segments.allocatable_count())
    }

    /// Free segments above which idle cleaning rests: half the log, and always more than the
    /// reserve so pressure cleaning keeps its room.
    fn idle_watermark(&self) -> u32 {
        (self.log.geometry.segment_count / 2).max(self.reserve.segments + 1)
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
    /// started victim can always be released (the progress contract). Idle cleaning also
    /// leaves the foreground reserve alone, so it can never push a write into `ENOSPC`.
    fn can_start_victim(&self, victim: u32, mode: SelectionMode) -> bool {
        let live = u64::from(self.log.segments.live_blocks(victim));
        let copies = live + live.div_ceil(MAXIMUM_PAYLOAD_BLOCKS as u64) + 1;
        let keep = match mode {
            SelectionMode::Pressure => 0,
            SelectionMode::Idle => self.reserve.segments,
        };
        self.maintenance_fits(copies, 2, keep)
    }

    fn maintain_step(&mut self, mode: SelectionMode) -> io::Result<MaintenanceStep> {
        self.poll_cleaner()?;
        // Ask for the next window before applying the last one, so the cleaner reads while
        // the writer copies.
        let dispatched = self.dispatch_scan()?;
        if let Some(buffer) = self.reclaim.queue.pop_front() {
            let result = self.apply_buffer(&buffer);
            self.reclaim.pool.push(buffer);
            result?;
            return Ok(MaintenanceStep::Applied);
        }
        if dispatched {
            return Ok(MaintenanceStep::Scanning);
        }
        match self.reclaim.scan {
            ScanState::InFlight => return Ok(MaintenanceStep::Waiting),
            // Every window buffer is held elsewhere; nothing can be read until one returns.
            ScanState::Ready(_) => return Ok(MaintenanceStep::Idle),
            ScanState::None => {}
        }
        self.offer_discards()?;
        if self.log.segments.pending_free_count() > 0 {
            return self.barrier_publication();
        }
        if mode == SelectionMode::Idle
            && self.log.segments.allocatable_count() >= self.idle_watermark()
        {
            // Plenty of free space: idle cleaning would only churn. Every barrier publication
            // leaves stale table pages behind, so without a floor on free space idle cleaning
            // feeds itself forever, one segment of stale metadata per cycle.
            return Ok(MaintenanceStep::Idle);
        }
        match select(&self.log, mode, None) {
            Selection::Victim(victim) if !self.can_start_victim(victim, mode) => {
                // Copies would eat into the space the freeing publications need.
                Ok(MaintenanceStep::Idle)
            }
            Selection::Victim(victim) => {
                self.refresh_view()?;
                let cursor = ScanCursor::start(
                    self.log.geometry,
                    victim,
                    self.log.segments.seal_lsn(victim),
                )?;
                self.reclaim.scan = ScanState::Ready(cursor);
                self.reclaim.stats.victims_selected += 1;
                Ok(if self.dispatch_scan()? {
                    MaintenanceStep::Scanning
                } else {
                    MaintenanceStep::Waiting
                })
            }
            // A recently sealed segment becomes eligible once both roots pass its seal LSN.
            Selection::AwaitPublication => self.barrier_publication(),
            Selection::Nothing if self.reclaim.discards_in_flight > 0 => {
                Ok(MaintenanceStep::Waiting)
            }
            Selection::Nothing => Ok(MaintenanceStep::Idle),
        }
    }

    /// Applies every event the cleaner has reported so far.
    fn poll_cleaner(&mut self) -> io::Result<()> {
        self.reclaim.cleaner.drain_wakeup();
        while let Some(event) = self.reclaim.cleaner.try_recv()? {
            self.handle_cleaner_event(event)?;
        }
        Ok(())
    }

    fn handle_cleaner_event(&mut self, event: Event) -> io::Result<()> {
        match event {
            Event::Scanned {
                cursor,
                buffer,
                report,
            } => {
                assert_eq!(self.reclaim.scan, ScanState::InFlight, "one scan at a time");
                self.reclaim.stats.windows_scanned += 1;
                self.reclaim.stats.scan_blocks_read += report.blocks_read;
                self.reclaim.stats.payloads_proposed += report.payloads_live;
                self.reclaim.stats.payloads_corrupt += report.payloads_corrupt;
                let sealed = self.log.segments.state(buffer.victim)
                    == crate::format_v4::segment_table::SegmentState::Sealed;
                if sealed {
                    self.reclaim.queue.push_back(buffer);
                } else {
                    // Overwrites emptied the victim during its scan: nothing left to apply.
                    self.reclaim.stats.proposals_rejected_source += 1;
                    self.reclaim.pool.push(buffer);
                }
                self.reclaim.scan = if cursor.done || !sealed {
                    ScanState::None
                } else {
                    ScanState::Ready(cursor)
                };
            }
            Event::ViewReturned(view) => {
                assert!(self.reclaim.spare_view.is_none(), "one view swap at a time");
                self.reclaim.spare_view = Some(view);
            }
            Event::Discarded { segment, result } => {
                self.reclaim.discards_in_flight -= 1;
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
            Event::ScanFailed(error) => return Err(error),
        }
        Ok(())
    }

    /// Hands the next window of the current victim to the cleaner when a buffer is free.
    fn dispatch_scan(&mut self) -> io::Result<bool> {
        let ScanState::Ready(cursor) = self.reclaim.scan else {
            return Ok(false);
        };
        let Some(buffer) = self.reclaim.pool.pop() else {
            return Ok(false);
        };
        self.reclaim.scan = ScanState::InFlight;
        self.reclaim
            .cleaner
            .send(Command::Scan { cursor, buffer })?;
        Ok(true)
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
        self.offer_discards()?;
        Ok(MaintenanceStep::Published { freed })
    }

    /// Copies the resident index into the spare view and swaps it into the cleaner when a
    /// newer root exists. An eligible victim sealed at or before both roots' LSNs has every
    /// write into it reflected in the active root, so a payload this view calls dead can never
    /// become live again; a payload it calls live may have died since, which the apply check
    /// catches. Commands are ordered, so the swap lands before the victim's first window.
    fn refresh_view(&mut self) -> io::Result<()> {
        let checkpoint_lsn = self.log.checkpoint_lsn();
        if self.reclaim.view_checkpoint_lsn == Some(checkpoint_lsn) {
            return Ok(());
        }
        let mut view = self
            .reclaim
            .spare_view
            .take()
            .expect("the previous swap returned before this victim's windows");
        self.log
            .index
            .snapshot_blocks()
            .iter()
            .zip(view.snapshot_block.iter_mut())
            .for_each(|(block, target)| *target = *block);
        view.maximum_lsn = self.log.next_lsn - 1;
        self.reclaim.cleaner.send(Command::Refresh(view))?;
        self.reclaim.view_checkpoint_lsn = Some(checkpoint_lsn);
        Ok(())
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
        if segments.state(victim) != crate::format_v4::segment_table::SegmentState::Sealed {
            // The last payload died while the windows were applied; the remap released it.
            return;
        }
        if segments.live_blocks(victim) == 0 && segments.metadata_blocks(victim) == 0 {
            segments
                .mark_pending_free(victim)
                .expect("sealed victim without references");
            self.reclaim.stats.victims_pending += 1;
        } else {
            // Something the scan could not move stays live: a payload whose bytes fail their
            // checksum, or a payload a buffered overwrite will replace. The next mapping change
            // that touches the segment clears the mark, and an emptied segment is released by
            // the remap itself.
            segments.mark_stuck(victim);
        }
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

    /// Hands segments freed by the last barrier to the cleaner for hole punching, at most
    /// `DISCARD_SLOTS` at a time, excluding each from allocation until its punch completes.
    /// A backing without hole punching disables further discards.
    fn offer_discards(&mut self) -> io::Result<()> {
        // Bounded by the segments marked due and the discard slots.
        loop {
            if self.reclaim.discard == BackingDiscard::Off || self.reclaim.discard_disabled {
                while self.log.segments.take_discard_due().is_some() {}
                return Ok(());
            }
            if self.reclaim.discards_in_flight >= DISCARD_SLOTS {
                return Ok(());
            }
            let Some(segment) = self.log.segments.take_discard_due() else {
                return Ok(());
            };
            if self.log.segments.state(segment)
                != crate::format_v4::segment_table::SegmentState::Free
            {
                // Reallocated while it waited for a discard slot.
                continue;
            }
            self.log.segments.begin_discard(segment)?;
            self.reclaim.discards_in_flight += 1;
            self.reclaim.cleaner.send(Command::Discard { segment })?;
        }
    }

    /// Runs maintenance under pressure until `demanded_blocks` fit or nothing can free space.
    /// A step that only waits for the cleaner blocks on its report instead of spinning. When
    /// nothing is left to clean while writes are buffered, the buffer is released once: a
    /// buffered overwrite keeps the victim's old payload live until it lands, so victims that
    /// ended their scan with such payloads are stuck exactly until then.
    pub(crate) fn reclaim_for_space(&mut self, demanded_blocks: u64) -> io::Result<()> {
        let mut released = false;
        // Bounded: every non-idle step scans, applies, publishes or waits for one report; the
        // buffer is released at most once; Idle ends the loop.
        loop {
            if ensure_user_space(&self.log, self.reserve, demanded_blocks).is_ok() {
                return Ok(());
            }
            self.reclaim.stats.pressure_steps += 1;
            match self.maintain(SelectionMode::Pressure)? {
                MaintenanceStep::Idle if !released && self.write_buffer.len() > 0 => {
                    released = true;
                    self.release_records(crate::ReleaseTrigger::Pressure)?;
                }
                MaintenanceStep::Idle => {
                    self.reclaim.stats.enospc_all_live += 1;
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
                MaintenanceStep::Waiting => {
                    self.reclaim.stats.pressure_waits += 1;
                    if !self.wait_for_cleaner(PRESSURE_WAIT)? {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "cleaner did not report within the pressure wait",
                        ));
                    }
                }
                _ => continue,
            }
        }
    }
}
