//! The writer's segment state: persistent fields, live counts and the two-publication barrier.
use super::{
    Geometry, HeadId, RecordHeader,
    geometry::TABLE_ENTRIES,
    segment_table::{SegmentEntry, SegmentState, SegmentTablePage},
};
use crate::{invalid_data, invalid_input};
use std::io;

/// Segment state as parallel arrays indexed by segment ID. Arrays are sized for `capacity`
/// segments so that backing growth never allocates.
pub(crate) struct Segments {
    geometry: Geometry,
    flags: Box<[u8]>,
    seal_lsn: Box<[u64]>,
    /// Flags as recorded by the active durable root; the barrier compares against them.
    published_flags: Box<[u8]>,
    /// Live payload blocks, rebuilt at open and adjusted on every mapping change.
    live_blocks: Box<[u32]>,
    /// Current metadata records (snapshots, index pages, table pages) located in the segment.
    metadata_blocks: Box<[u32]>,
    /// Blocks written since the segment was opened; the link counts, unused tail does not.
    used_blocks: Box<[u32]>,
    /// An authorized backing discard is outstanding; the allocator skips the segment.
    discard_pending: Box<[bool]>,
    /// Freed by the last barrier and not yet offered for backing discard.
    discard_due: Box<[bool]>,
    /// A completed scan left references the cleaner cannot move; cleared when live counts
    /// change so the segment becomes a candidate again.
    stuck: Box<[bool]>,
    /// Sealed segments that writes or metadata moves emptied without a scan.
    emptied_count: u64,
    table_block: Box<[u32]>,
    table_dirty: Box<[bool]>,
    free_count: u32,
    pending_free_count: u32,
    discard_pending_count: u32,
}

/// The persistent image of one segment table page, ready for encoding.
pub(crate) struct TablePageImage {
    pub(crate) entries: [SegmentEntry; TABLE_ENTRIES],
}

impl Segments {
    /// Preallocates state for `capacity` segments; `geometry.segment_count` of them exist now.
    pub(crate) fn new(geometry: Geometry, capacity: u32) -> io::Result<Self> {
        if capacity < geometry.segment_count || u64::from(capacity) > super::geometry::MAX_SEGMENTS
        {
            return Err(invalid_input("segment state capacity is invalid"));
        }
        let table_capacity = capacity.div_ceil(TABLE_ENTRIES as u32) as usize;
        let count = capacity as usize;
        let mut segments = Self {
            geometry,
            flags: filled(count, 0)?,
            seal_lsn: filled(count, 0)?,
            published_flags: filled(count, 0)?,
            live_blocks: filled(count, 0)?,
            metadata_blocks: filled(count, 0)?,
            used_blocks: filled(count, 0)?,
            discard_pending: filled(count, false)?,
            discard_due: filled(count, false)?,
            stuck: filled(count, false)?,
            table_block: filled(table_capacity, 0)?,
            table_dirty: filled(table_capacity, false)?,
            free_count: geometry.segment_count,
            pending_free_count: 0,
            discard_pending_count: 0,
            emptied_count: 0,
        };
        segments.table_dirty[..geometry.table_pages as usize].fill(true);
        Ok(segments)
    }

    #[cfg(test)]
    pub(crate) fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub(crate) fn capacity(&self) -> u32 {
        u32::try_from(self.flags.len()).expect("bounded")
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        std::mem::size_of_val(self.flags.as_ref())
            + std::mem::size_of_val(self.seal_lsn.as_ref())
            + std::mem::size_of_val(self.published_flags.as_ref())
            + std::mem::size_of_val(self.live_blocks.as_ref())
            + std::mem::size_of_val(self.metadata_blocks.as_ref())
            + std::mem::size_of_val(self.used_blocks.as_ref())
            + std::mem::size_of_val(self.discard_pending.as_ref())
            + std::mem::size_of_val(self.discard_due.as_ref())
            + std::mem::size_of_val(self.stuck.as_ref())
            + std::mem::size_of_val(self.table_block.as_ref())
            + std::mem::size_of_val(self.table_dirty.as_ref())
    }

    pub(crate) fn entry(&self, segment: u32) -> io::Result<SegmentEntry> {
        self.check(segment)?;
        SegmentEntry::from_fields(
            self.flags[segment as usize],
            self.seal_lsn[segment as usize],
        )
    }

    pub(crate) fn state(&self, segment: u32) -> SegmentState {
        self.entry(segment)
            .expect("segment state is always valid")
            .state
    }

    pub(crate) fn seal_lsn(&self, segment: u32) -> u64 {
        self.seal_lsn[segment as usize]
    }

    pub(crate) fn live_blocks(&self, segment: u32) -> u32 {
        self.live_blocks[segment as usize]
    }

    pub(crate) fn metadata_blocks(&self, segment: u32) -> u32 {
        self.metadata_blocks[segment as usize]
    }

    pub(crate) fn used_blocks(&self, segment: u32) -> u32 {
        self.used_blocks[segment as usize]
    }

    pub(crate) fn free_count(&self) -> u32 {
        self.free_count
    }

    pub(crate) fn pending_free_count(&self) -> u32 {
        self.pending_free_count
    }

    /// Free segments the allocator can hand out now.
    pub(crate) fn allocatable_count(&self) -> u32 {
        self.free_count - self.discard_pending_count
    }

    pub(crate) fn table_block(&self, table: u32) -> u32 {
        self.table_block[table as usize]
    }

    pub(crate) fn table_dirty(&self, table: u32) -> bool {
        self.table_dirty[table as usize]
    }

    /// Forces the next publication to rewrite `table`, moving it out of a victim.
    pub(crate) fn mark_table_dirty(&mut self, table: u32) {
        self.table_dirty[table as usize] = true;
    }

    pub(crate) fn is_stuck(&self, segment: u32) -> bool {
        self.stuck[segment as usize]
    }

    pub(crate) fn mark_stuck(&mut self, segment: u32) {
        self.stuck[segment as usize] = true;
    }

    /// The next segment freed by a barrier that has not been offered for discard.
    pub(crate) fn take_discard_due(&mut self) -> Option<u32> {
        let segment = self.discard_due.iter().position(|due| *due)?;
        self.discard_due[segment] = false;
        Some(u32::try_from(segment).expect("bounded"))
    }

    pub(crate) fn dirty_table_pages(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.geometry.table_pages).filter(|table| self.table_dirty[*table as usize])
    }

    pub(crate) fn dirty_table_page_count(&self) -> u32 {
        self.dirty_table_pages().count() as u32
    }

    fn check(&self, segment: u32) -> io::Result<()> {
        if segment >= self.geometry.segment_count {
            return Err(invalid_data("segment ID out of range"));
        }
        Ok(())
    }

    fn set(&mut self, segment: u32, entry: SegmentEntry) {
        let index = segment as usize;
        self.flags[index] = entry.flags();
        self.seal_lsn[index] = entry.seal_lsn;
        let (table, _) = self.geometry.table_slot(segment);
        self.table_dirty[table as usize] = true;
    }

    /// Installs the state recorded by a validated root before replay adjusts it.
    pub(crate) fn load_entry(&mut self, segment: u32, entry: SegmentEntry) -> io::Result<()> {
        self.check(segment)?;
        entry.validate()?;
        let index = segment as usize;
        let was_free = self.flags[index] == SegmentEntry::FREE.flags();
        if !was_free {
            return Err(invalid_data("segment loaded twice"));
        }
        self.flags[index] = entry.flags();
        self.seal_lsn[index] = entry.seal_lsn;
        self.published_flags[index] = entry.flags();
        match entry.state {
            SegmentState::Free => {}
            SegmentState::PendingFree => {
                self.free_count -= 1;
                self.pending_free_count += 1;
                self.used_blocks[index] = self.geometry.blocks_per_segment;
            }
            SegmentState::Sealed => {
                self.free_count -= 1;
                self.used_blocks[index] = self.geometry.blocks_per_segment;
            }
            SegmentState::Open => self.free_count -= 1,
        }
        Ok(())
    }

    /// Records the table page locations of the loaded root; pages then start clean.
    pub(crate) fn load_table_blocks(&mut self, blocks: &[u32]) -> io::Result<()> {
        if blocks.len() != self.geometry.table_pages as usize {
            return Err(invalid_data("table page pointer count mismatch"));
        }
        for (table, &block) in blocks.iter().enumerate() {
            self.table_block[table] = block;
            self.table_dirty[table] = false;
            if block != 0 {
                self.add_metadata(block)?;
            }
        }
        Ok(())
    }

    /// The lowest allocatable free segment, opened for `head`.
    pub(crate) fn allocate(&mut self, head: HeadId) -> io::Result<u32> {
        let segment = (0..self.geometry.segment_count)
            .find(|&segment| {
                self.flags[segment as usize] == SegmentEntry::FREE.flags()
                    && !self.discard_pending[segment as usize]
            })
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSPC))?;
        self.set(segment, SegmentEntry::open(head));
        self.free_count -= 1;
        self.used_blocks[segment as usize] = 0;
        // A segment reused before its punch was offered holds live data again: no discard.
        self.discard_due[segment as usize] = false;
        Ok(segment)
    }

    /// Replay saw a link into `segment`. Free and PendingFree segments may open again: a
    /// PendingFree segment is one the recovered root no longer depends on (T7).
    pub(crate) fn replay_open(&mut self, segment: u32, head: HeadId) -> io::Result<()> {
        let entry = self.entry(segment)?;
        match entry.state {
            SegmentState::Free => self.free_count -= 1,
            SegmentState::PendingFree => self.pending_free_count -= 1,
            SegmentState::Open | SegmentState::Sealed => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "replayed link opens segment {segment} in use ({:?}, head {:?}, live {}, metadata {})",
                        entry.state,
                        entry.head,
                        self.live_blocks[segment as usize],
                        self.metadata_blocks[segment as usize]
                    ),
                ));
            }
        }
        if self.discard_pending[segment as usize] {
            return Err(invalid_data(
                "replay cannot open a segment with a pending discard",
            ));
        }
        self.set(segment, SegmentEntry::open(head));
        self.used_blocks[segment as usize] = 0;
        Ok(())
    }

    pub(crate) fn seal(&mut self, segment: u32, head: HeadId, seal_lsn: u64) -> io::Result<()> {
        let entry = self.entry(segment)?;
        if entry.state != SegmentState::Open || entry.head != Some(head) {
            return Err(invalid_data(
                "only the owning head can seal an open segment",
            ));
        }
        self.set(segment, SegmentEntry::sealed(seal_lsn));
        Ok(())
    }

    /// Accounts blocks appended to an open segment.
    pub(crate) fn record_used(&mut self, segment: u32, blocks: u32) {
        let used = &mut self.used_blocks[segment as usize];
        *used += blocks;
        assert!(
            *used <= self.geometry.blocks_per_segment,
            "segment {segment} overfilled"
        );
    }

    /// A sealed segment without live payloads or current metadata enters the barrier.
    /// A sealed segment that no longer holds a live payload or current metadata has no
    /// references left; it goes straight to `PendingFree` and the barrier frees it. This is how
    /// victims whose last payloads died after their scan, and segments overwrites emptied on
    /// their own, are released without another scan. Only user and relocation remaps call
    /// this: metadata moves happen inside a publication, after its table images were built,
    /// and a state change there would desynchronize `published_flags` from the durable root.
    pub(crate) fn release_if_empty(&mut self, segment: u32) -> io::Result<bool> {
        let index = segment as usize;
        if self.state(segment) != SegmentState::Sealed
            || self.live_blocks[index] != 0
            || self.metadata_blocks[index] != 0
        {
            return Ok(false);
        }
        self.mark_pending_free(segment)?;
        self.emptied_count += 1;
        Ok(true)
    }

    pub(crate) fn emptied_count(&self) -> u64 {
        self.emptied_count
    }

    pub(crate) fn mark_pending_free(&mut self, segment: u32) -> io::Result<()> {
        let entry = self.entry(segment)?;
        let index = segment as usize;
        if entry.state != SegmentState::Sealed
            || self.live_blocks[index] != 0
            || self.metadata_blocks[index] != 0
        {
            return Err(invalid_input("segment is not ready to be freed"));
        }
        self.set(segment, SegmentEntry::pending_free(entry.seal_lsn));
        self.pending_free_count += 1;
        Ok(())
    }

    /// The entry the next publication records: a segment pending in the active root becomes free.
    fn publication_entry(&self, segment: u32) -> SegmentEntry {
        let index = segment as usize;
        let entry = self.entry(segment).expect("valid state");
        let pending = SegmentEntry::pending_free(entry.seal_lsn).flags();
        if entry.state == SegmentState::PendingFree && self.published_flags[index] == pending {
            SegmentEntry::FREE
        } else {
            entry
        }
    }

    pub(crate) fn table_page_image(&self, table: u32) -> io::Result<TablePageImage> {
        if table >= self.geometry.table_pages {
            return Err(invalid_data("table page out of range"));
        }
        let mut image = TablePageImage {
            entries: [SegmentEntry::FREE; TABLE_ENTRIES],
        };
        let first = table * TABLE_ENTRIES as u32;
        for slot in 0..self.geometry.table_children(table) {
            image.entries[slot] = self.publication_entry(first + slot as u32);
        }
        Ok(image)
    }

    pub(crate) fn encode_table_page(
        &self,
        header: RecordHeader,
        table: u32,
    ) -> io::Result<SegmentTablePage> {
        let image = self.table_page_image(table)?;
        let mut page = SegmentTablePage::empty(header, table);
        for (slot, entry) in image.entries.iter().enumerate() {
            page.set(slot, *entry);
        }
        Ok(page)
    }

    /// A table page record for `table` completed at `block`; the old record is no longer current.
    pub(crate) fn table_page_written(&mut self, table: u32, block: u32) -> io::Result<()> {
        let old = self.table_block[table as usize];
        self.table_block[table as usize] = block;
        self.table_dirty[table as usize] = false;
        self.move_metadata(old, block)
    }

    /// The descriptor naming the current table pages is durable: apply barrier transitions.
    /// Returns how many segments became free.
    pub(crate) fn checkpoint_published(&mut self) -> io::Result<u32> {
        let mut freed = 0;
        for segment in 0..self.geometry.segment_count {
            let index = segment as usize;
            let recorded = self.publication_entry(segment);
            if recorded.state == SegmentState::Free
                && self.state(segment) == SegmentState::PendingFree
            {
                self.flags[index] = SegmentEntry::FREE.flags();
                self.seal_lsn[index] = 0;
                self.used_blocks[index] = 0;
                self.stuck[index] = false;
                self.discard_due[index] = true;
                self.pending_free_count -= 1;
                self.free_count += 1;
                freed += 1;
            }
            self.published_flags[index] = self.flags[index];
        }
        // The durable root now matches `published_flags`, so state may change again: sealed
        // segments whose last current metadata this publication re-emitted hold nothing.
        for segment in 0..self.geometry.segment_count {
            self.release_if_empty(segment)?;
        }
        Ok(freed)
    }

    /// Returns the segments freed by the last barrier step so a caller can preallocate.
    pub(crate) fn published_free_transitions(&self) -> u32 {
        (0..self.geometry.segment_count)
            .filter(|&segment| {
                self.publication_entry(segment).state == SegmentState::Free
                    && self.state(segment) == SegmentState::PendingFree
            })
            .count() as u32
    }

    pub(crate) fn begin_discard(&mut self, segment: u32) -> io::Result<()> {
        if self.state(segment) != SegmentState::Free || self.discard_pending[segment as usize] {
            return Err(invalid_input("only a free segment can be discarded once"));
        }
        self.discard_pending[segment as usize] = true;
        self.discard_pending_count += 1;
        Ok(())
    }

    pub(crate) fn finish_discard(&mut self, segment: u32) {
        assert!(
            self.discard_pending[segment as usize],
            "discard completion without a pending discard"
        );
        self.discard_pending[segment as usize] = false;
        self.discard_pending_count -= 1;
    }

    pub(crate) fn add_live(&mut self, block: u32) -> io::Result<()> {
        let segment = self.geometry.segment_of(block)? as usize;
        self.live_blocks[segment] += 1;
        Ok(())
    }

    pub(crate) fn remove_live(&mut self, block: u32) -> io::Result<()> {
        let segment = self.geometry.segment_of(block)? as usize;
        self.live_blocks[segment] = self.live_blocks[segment]
            .checked_sub(1)
            .ok_or_else(|| invalid_data("live block count underflow"))?;
        self.stuck[segment] = false;
        Ok(())
    }

    pub(crate) fn add_metadata(&mut self, block: u32) -> io::Result<()> {
        let segment = self.geometry.segment_of(block)? as usize;
        self.metadata_blocks[segment] += 1;
        Ok(())
    }

    /// Moves one current metadata reference; either side may be zero (absent).
    pub(crate) fn move_metadata(&mut self, old_block: u32, new_block: u32) -> io::Result<()> {
        if old_block != 0 {
            let segment = self.geometry.segment_of(old_block)? as usize;
            self.metadata_blocks[segment] = self.metadata_blocks[segment]
                .checked_sub(1)
                .ok_or_else(|| invalid_data("metadata reference underflow"))?;
            // A victim whose scan left only metadata behind waits for this re-emission; the
            // state itself changes after the publication (`release_emptied`).
            self.stuck[segment] = false;
        }
        if new_block != 0 {
            self.add_metadata(new_block)?;
        }
        Ok(())
    }

    /// Grows to `geometry`; new whole segments start free. Fails before any state changes when
    /// the preallocated capacity cannot hold them.
    pub(crate) fn grow(&mut self, geometry: Geometry) -> io::Result<()> {
        if geometry.segment_count > self.capacity() {
            return Err(invalid_input(
                "backing growth exceeds the preallocated segment state",
            ));
        }
        if geometry.segment_count < self.geometry.segment_count {
            return Err(invalid_data("backing cannot shrink"));
        }
        let added = geometry.segment_count - self.geometry.segment_count;
        for table in self.geometry.table_pages..geometry.table_pages {
            self.table_dirty[table as usize] = true;
        }
        if added != 0 {
            let (table, _) = geometry.table_slot(self.geometry.segment_count);
            self.table_dirty[table as usize] = true;
        }
        self.free_count += added;
        self.geometry = geometry;
        Ok(())
    }

    /// Whether the counts still describe the segments: every segment is in one state and no
    /// live or metadata block exceeds what was written there.
    #[cfg(test)]
    pub(crate) fn check_conservation(&self) -> io::Result<()> {
        let mut free = 0;
        let mut pending = 0;
        let mut discards = 0;
        for segment in 0..self.geometry.segment_count {
            let index = segment as usize;
            let entry = self.entry(segment)?;
            match entry.state {
                SegmentState::Free => {
                    free += 1;
                    if self.live_blocks[index] != 0 || self.metadata_blocks[index] != 0 {
                        return Err(invalid_data("free segment still referenced"));
                    }
                }
                SegmentState::PendingFree => {
                    pending += 1;
                    if self.live_blocks[index] != 0 || self.metadata_blocks[index] != 0 {
                        return Err(invalid_data("pending segment still referenced"));
                    }
                }
                SegmentState::Open | SegmentState::Sealed => {}
            }
            discards += u32::from(self.discard_pending[index]);
            if self.live_blocks[index] + self.metadata_blocks[index] > self.used_blocks[index] {
                return Err(invalid_data("segment references exceed its written blocks"));
            }
        }
        if free != self.free_count
            || pending != self.pending_free_count
            || discards != self.discard_pending_count
        {
            return Err(invalid_data("segment counters disagree with states"));
        }
        Ok(())
    }
}

pub(crate) fn filled<T: Clone>(length: usize, value: T) -> io::Result<Box<[T]>> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(length)
        .map_err(|_| io::Error::other("segment state allocation failed"))?;
    entries.resize(length, value);
    Ok(entries.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v4::{NO_PREVIOUS_BOUNDARY, RecordKind};

    fn geometry() -> Geometry {
        Geometry::new(1000, 2 + 6 * 8, 8).unwrap()
    }

    fn fresh() -> Segments {
        let geometry = geometry();
        let mut segments = Segments::new(geometry, 8).unwrap();
        assert_eq!(segments.allocate(HeadId::User).unwrap(), 0);
        assert_eq!(segments.allocate(HeadId::Relocation).unwrap(), 1);
        segments
    }

    #[test]
    fn allocation_takes_the_lowest_free_segment_and_skips_pending_discards() {
        let mut segments = fresh();
        assert_eq!(segments.free_count(), 4);
        assert_eq!(segments.entry(0).unwrap(), SegmentEntry::open(HeadId::User));
        assert_eq!(
            segments.entry(1).unwrap(),
            SegmentEntry::open(HeadId::Relocation)
        );
        segments.begin_discard(2).unwrap();
        assert_eq!(segments.allocatable_count(), 3);
        assert_eq!(segments.allocate(HeadId::User).unwrap(), 3);
        segments.finish_discard(2);
        assert_eq!(segments.allocate(HeadId::User).unwrap(), 2);
        assert_eq!(segments.allocate(HeadId::User).unwrap(), 4);
        assert_eq!(segments.allocate(HeadId::User).unwrap(), 5);
        assert_eq!(
            segments.allocate(HeadId::User).unwrap_err().raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert!(segments.begin_discard(0).is_err());
        assert!(segments.seal(0, HeadId::Relocation, 5).is_err());
        segments.seal(0, HeadId::User, 5).unwrap();
        assert!(segments.seal(0, HeadId::User, 6).is_err());
        assert_eq!(segments.entry(0).unwrap(), SegmentEntry::sealed(5));
        segments.check_conservation().unwrap();
        assert!(segments.entry(6).is_err());
        assert!(Segments::new(geometry(), 5).is_err());
    }

    #[test]
    fn freeing_needs_two_publications_and_republishes_pending_state_after_fallback() {
        let mut segments = fresh();
        segments.record_used(0, 8);
        segments.seal(0, HeadId::User, 9).unwrap();
        segments.add_live(2).unwrap();
        assert!(segments.mark_pending_free(0).is_err());
        segments.remove_live(2).unwrap();
        assert!(segments.remove_live(2).is_err());
        segments.mark_pending_free(0).unwrap();
        assert_eq!(segments.pending_free_count(), 1);
        // Publication A records PendingFree: the active root did not know the segment as pending.
        let image = segments.table_page_image(0).unwrap();
        assert_eq!(image.entries[0], SegmentEntry::pending_free(9));
        assert_eq!(segments.published_free_transitions(), 0);
        assert_eq!(segments.checkpoint_published().unwrap(), 0);
        assert_eq!(segments.state(0), SegmentState::PendingFree);
        // Publication B records Free and the segment becomes allocatable after the descriptor.
        assert_eq!(
            segments.table_page_image(0).unwrap().entries[0],
            SegmentEntry::FREE
        );
        assert_eq!(segments.published_free_transitions(), 1);
        assert_eq!(segments.checkpoint_published().unwrap(), 1);
        assert_eq!(segments.take_discard_due(), Some(0));
        assert_eq!(segments.take_discard_due(), None);
        assert_eq!(segments.entry(0).unwrap(), SegmentEntry::FREE);
        assert_eq!(segments.free_count(), 5);
        assert_eq!(segments.used_blocks(0), 0);
        segments.check_conservation().unwrap();

        // Loading a root that still records PendingFree keeps the barrier at one step.
        let mut recovered = Segments::new(geometry(), 6).unwrap();
        recovered
            .load_entry(0, SegmentEntry::pending_free(9))
            .unwrap();
        recovered
            .load_entry(1, SegmentEntry::open(HeadId::User))
            .unwrap();
        recovered
            .load_entry(2, SegmentEntry::open(HeadId::Relocation))
            .unwrap();
        recovered.load_entry(3, SegmentEntry::sealed(4)).unwrap();
        assert!(recovered.load_entry(3, SegmentEntry::sealed(4)).is_err());
        assert_eq!(recovered.free_count(), 2);
        assert_eq!(recovered.pending_free_count(), 1);
        assert_eq!(recovered.used_blocks(3), 8);
        assert_eq!(recovered.published_free_transitions(), 1);
        // A replayed link may reopen the pending segment (T7); a sealed one it may not.
        recovered.replay_open(0, HeadId::User).unwrap();
        assert_eq!(
            recovered.entry(0).unwrap(),
            SegmentEntry::open(HeadId::User)
        );
        assert_eq!(recovered.pending_free_count(), 0);
        assert!(recovered.replay_open(3, HeadId::User).is_err());
        recovered.replay_open(4, HeadId::Relocation).unwrap();
        assert_eq!(recovered.free_count(), 1);
        recovered.check_conservation().unwrap();
    }

    #[test]
    fn table_pages_track_dirty_state_metadata_references_and_growth() {
        let mut segments = fresh();
        assert_eq!(segments.dirty_table_pages().collect::<Vec<_>>(), [0]);
        let header = RecordHeader {
            kind: RecordKind::SegmentTablePage,
            volume_id: 1,
            lsn: 3,
            previous_boundary: NO_PREVIOUS_BOUNDARY,
            boundary: 2,
        };
        let page = segments.encode_table_page(header, 0).unwrap();
        assert_eq!(page.entry(0).unwrap(), SegmentEntry::open(HeadId::User));
        assert_eq!(page.entry(5).unwrap(), SegmentEntry::FREE);
        assert_eq!(page.entry(6).unwrap(), SegmentEntry::FREE);
        segments.record_used(0, 1);
        segments.table_page_written(0, 2).unwrap();
        assert_eq!(segments.dirty_table_page_count(), 0);
        assert_eq!(segments.metadata_blocks(0), 1);
        segments.check_conservation().unwrap();
        segments.record_used(0, 1);
        segments.table_page_written(0, 3).unwrap();
        assert_eq!(segments.metadata_blocks(0), 1);
        assert_eq!(segments.table_block(0), 3);
        assert!(segments.move_metadata(20, 0).is_err());
        segments.move_metadata(3, 0).unwrap();
        assert_eq!(segments.metadata_blocks(0), 0);
        assert!(segments.move_metadata(3, 0).is_err());

        assert!(
            segments
                .grow(Geometry::new(1000, 2 + 9 * 8, 8).unwrap())
                .is_err()
        );
        assert!(
            segments
                .grow(Geometry::new(1000, 2 + 5 * 8, 8).unwrap())
                .is_err()
        );
        segments
            .grow(Geometry::new(1000, 2 + 8 * 8 + 3, 8).unwrap())
            .unwrap();
        assert_eq!(segments.geometry().segment_count, 8);
        assert_eq!(segments.free_count(), 6);
        assert!(segments.table_dirty(0));
        assert_eq!(segments.allocate(HeadId::User).unwrap(), 2);
        segments.check_conservation().unwrap();

        let mut loaded = Segments::new(geometry(), 6).unwrap();
        loaded.load_table_blocks(&[2]).unwrap();
        assert!(loaded.load_table_blocks(&[2, 3]).is_err());
        assert_eq!(loaded.metadata_blocks(0), 1);
        assert!(!loaded.table_dirty(0));
    }

    #[test]
    fn conservation_detects_dangling_references() {
        let mut segments = fresh();
        segments.record_used(0, 2);
        segments.add_live(2).unwrap();
        segments.add_live(3).unwrap();
        segments.check_conservation().unwrap();
        segments.add_live(4).unwrap();
        assert!(segments.check_conservation().is_err());
        assert_eq!(
            segments.allocated_bytes(),
            8 * (1 + 8 + 1 + 4 + 4 + 4 + 1 + 1 + 1) + 4 + 1
        );
    }
}
