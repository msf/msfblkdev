use super::*;
use crate::format_v4::{
    RecordKind,
    reservation::{Reservation, ensure_user_space},
    small_records::LbaDiscard,
};

impl Volume {
    /// Unmaps `count` blocks from `lba`. Buffered writes inside the range are dropped first, so
    /// request order wins: a discard after a buffered write removes that value (review T5).
    /// Reads return zeros afterwards; FLUSH makes the discard durable like a write.
    pub fn discard_blocks(&mut self, lba: u32, count: u32) -> io::Result<()> {
        self.check_operational()?;
        let volume_blocks = self.volume_blocks();
        if count == 0 || lba > volume_blocks || count > volume_blocks - lba {
            return Err(invalid_input("invalid discard range"));
        }
        if self.write_buffer.remove_range(lba, count) {
            self.buffered_pages = self.write_buffer.distinct_pages();
        }
        let capacity = self.mapping.cache.snapshot().sizing.capacity as u32;
        let mut start = lba;
        let end = lba + count;
        // Bounded: each chunk unmaps at least one page.
        while start < end {
            let first_page = start / ENTRIES_PER_PAGE as u32;
            let last_page = ((end - 1) / ENTRIES_PER_PAGE as u32).min(first_page + capacity - 1);
            let chunk_end = end.min((last_page + 1) * ENTRIES_PER_PAGE as u32);
            let result = self.discard_chunk(start, chunk_end - start, first_page..=last_page);
            self.latch(result)?;
            start = chunk_end;
        }
        Ok(())
    }

    /// Write-zeroes has the same durable effect as a discard on this device: reads return
    /// zeros. NOUNMAP hints are honored by unmapping anyway, which preserves the contract.
    pub fn write_zeroes(&mut self, lba: u32, count: u32) -> io::Result<()> {
        self.discard_blocks(lba, count)
    }

    fn discard_chunk(
        &mut self,
        lba: u32,
        count: u32,
        pages: std::ops::RangeInclusive<u32>,
    ) -> io::Result<()> {
        let page_count = (*pages.end() - *pages.start() + 1) as usize;
        let capacity = self.mapping.cache.snapshot().sizing.capacity;
        assert!(page_count <= capacity, "chunk fits the cache");
        // Every page must be unmapped before any checkpoint can pass the record's LSN.
        let unrelated_dirty = self
            .mapping
            .cache
            .dirty_pages()
            .filter(|(page_id, _)| !pages.contains(page_id))
            .count();
        if unrelated_dirty + page_count > capacity {
            self.mapping
                .publish(&mut self.backing_io, &self.backing, &mut self.log)?;
            self.mapping.pressure_checkpoints += 1;
        }
        let reservation =
            Reservation::for_metadata(1, page_count, self.mapping.cache.dirty_pages().count())?;
        let demanded = reservation.demanded_blocks(self.log.geometry) + self.maintenance_debt();
        if ensure_user_space(&self.log, self.reserve, demanded).is_err() {
            self.reclaim_for_space(demanded)?;
        }
        self.log.append_metadata(
            &mut self.backing_io,
            &self.backing,
            HeadId::User,
            RecordKind::LbaDiscard,
            |log, header, bytes| {
                let mut record = LbaDiscard::empty(header);
                assert!(record.push(lba, count));
                record.encode(log.geometry, bytes)
            },
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("discard-write-complete")?;
        for page_id in pages {
            let first =
                (lba.max(page_id * ENTRIES_PER_PAGE as u32) % ENTRIES_PER_PAGE as u32) as usize;
            let last = ((lba + count - 1)
                .min(page_id * ENTRIES_PER_PAGE as u32 + ENTRIES_PER_PAGE as u32 - 1)
                % ENTRIES_PER_PAGE as u32) as usize;
            let mut unmapped = [0_u32; ENTRIES_PER_PAGE];
            let applied = self.mapping.with_page(
                &mut self.backing_io,
                &self.backing,
                &mut self.log,
                page_id,
                CheckpointPolicy::Forbid,
                |mut page| {
                    let mut applied = 0;
                    for entry in first..=last {
                        let old = page.entries().physical_addr[entry];
                        if old == 0 {
                            continue;
                        }
                        page.update(entry, 0, 0).map_err(|error| {
                            io::Error::other(format!("mapping update: {error:?}"))
                        })?;
                        unmapped[applied] = old;
                        applied += 1;
                    }
                    Ok(applied)
                },
            )?;
            for &old in &unmapped[..applied] {
                self.remap(old, 0)?;
            }
        }
        Ok(())
    }
}
