use super::*;
use crate::format_v4::{
    RecordHeader,
    data_record::DataRecord,
    reservation::{Reservation, ensure_user_space},
};
use crate::{MAXIMUM_PAYLOAD_BLOCKS, accounted_io::submit_exact};
use io_uring::{opcode, types};
use std::os::fd::AsRawFd;

impl Volume {
    pub fn write_block(&mut self, lba: u32, data: &[u8; BLOCK_SIZE]) -> io::Result<()> {
        self.check_operational()?;
        if lba >= self.volume_blocks() {
            return Err(invalid_input("invalid logical block"));
        }
        if self.write_buffer.get(lba).is_some() {
            self.write_buffer.store(lba, data);
            return Ok(());
        }
        let reservation = Reservation::admit(
            &self.write_buffer.lbas,
            self.buffered_pages,
            self.mapping.cache.dirty_pages().count(),
            lba,
        )?;
        ensure_user_space(
            &self.log,
            self.reserve,
            reservation.demanded_blocks(self.log.geometry),
        )?;
        assert!(self.write_buffer.store(lba, data), "new buffered LBA");
        self.buffered_pages = reservation.pending_pages as usize;
        if self.write_buffer.len() == self.write_buffer.capacity() {
            self.release_records(ReleaseTrigger::Full)?;
        }
        Ok(())
    }

    pub(super) fn release_records(&mut self, trigger: ReleaseTrigger) -> io::Result<()> {
        if self.write_buffer.len() == 0 {
            return Ok(());
        }
        match trigger {
            ReleaseTrigger::Full => self.release_stats.full_releases += 1,
            ReleaseTrigger::Flush => self.release_stats.flush_releases += 1,
        }
        self.write_buffer.sort_by_lba();
        let preparation = self.prepare_release();
        self.latch(preparation)?;
        let mut start = 0;
        // Bounded by the buffer: every iteration writes payloads or opens a segment with room.
        while start < self.write_buffer.len() {
            let room = self.log.head(HeadId::User).payload_room(self.log.geometry);
            if room == 0 {
                let advanced =
                    self.log
                        .advance_head(&mut self.backing_io, &self.backing, HeadId::User);
                self.latch(advanced)?;
                continue;
            }
            let end = (start + room.min(MAXIMUM_PAYLOAD_BLOCKS)).min(self.write_buffer.len());
            let result = self.write_record(start..end);
            self.latch(result)?;
            start = end;
        }
        self.write_buffer.clear();
        self.buffered_pages = 0;
        Ok(())
    }

    /// Publishes first when the group's pages would not fit the cache or the replay bound.
    fn prepare_release(&mut self) -> io::Result<()> {
        let payloads = self.write_buffer.len();
        let record_bytes =
            (payloads + payloads.div_ceil(MAXIMUM_PAYLOAD_BLOCKS)) as u64 * BLOCK_SIZE as u64;
        let since_checkpoint = self.log.blocks_since_checkpoint * BLOCK_SIZE as u64;
        let unrelated_dirty = self
            .mapping
            .cache
            .dirty_pages()
            .filter(|(page_id, _)| {
                self.write_buffer
                    .lbas
                    .binary_search_by(|lba| (lba / ENTRIES_PER_PAGE as u32).cmp(page_id))
                    .is_err()
            })
            .count();
        let pressure =
            unrelated_dirty + self.buffered_pages > self.mapping.cache.snapshot().sizing.capacity;
        if pressure || since_checkpoint + record_bytes > self.checkpoint_after_bytes {
            self.mapping
                .publish(&mut self.backing_io, &self.backing, &mut self.log)?;
            if pressure {
                self.mapping.pressure_checkpoints += 1;
            }
        }
        Ok(())
    }

    fn write_record(&mut self, range: std::ops::Range<usize>) -> io::Result<()> {
        let count = range.len();
        let header = self.log.data_header(HeadId::User, count);
        let record = self.encode_record(header, range.clone())?;
        self.submit_record(&record)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::RECORD_WRITE_COMPLETE_FAILPOINT)?;
        self.log.data_committed(HeadId::User, header)?;
        self.install_record_mappings(&record, count)?;
        self.release_stats.record(count);
        Ok(())
    }

    fn encode_record(
        &mut self,
        header: RecordHeader,
        range: std::ops::Range<usize>,
    ) -> io::Result<DataRecord> {
        let start = header.start(self.log.geometry)?;
        let mut record = DataRecord::empty(header);
        for (index, &lba) in self.write_buffer.lbas[range].iter().enumerate() {
            record.lba_ids[index] = lba;
            record.checksums[index] = payload_checksum(
                self.log.volume_id,
                lba,
                start + index as u32,
                self.write_buffer
                    .get(lba)
                    .expect("buffered payload retains its arena slot"),
            );
        }
        record.encode(self.log.geometry, &mut self.footer_scratch.0)?;
        Ok(record)
    }

    fn submit_record(&mut self, record: &DataRecord) -> io::Result<()> {
        let count = record.header.payload_count(self.log.geometry)?;
        let start = record.header.start(self.log.geometry)?;
        let iovecs = &mut self.release_iovecs.0;
        iovecs.clear();
        for &lba in record.lba_ids.iter().take(count) {
            let payload = self
                .write_buffer
                .get(lba)
                .expect("buffered payload retains its arena slot");
            iovecs.push(libc::iovec {
                iov_base: payload.as_ptr() as *mut libc::c_void,
                iov_len: BLOCK_SIZE,
            });
        }
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
            record.header.lsn,
            ((count + 1) * BLOCK_SIZE) as i32,
        )
    }

    /// Installs the mappings of a completed record, one mapping page at a time.
    pub(crate) fn install_record_mappings(
        &mut self,
        record: &DataRecord,
        count: usize,
    ) -> io::Result<()> {
        let geometry = self.log.geometry;
        let start = record.header.start(geometry)?;
        let mut first = 0;
        while first < count {
            let page_id = record.lba_ids[first] / ENTRIES_PER_PAGE as u32;
            let end = first
                + record.lba_ids[first..count]
                    .partition_point(|lba| *lba / ENTRIES_PER_PAGE as u32 == page_id);
            let mut remaps = [(0_u32, 0_u32); MAXIMUM_PAYLOAD_BLOCKS];
            let applied = self.mapping.with_page(
                &mut self.backing_io,
                &self.backing,
                &mut self.log,
                page_id,
                CheckpointPolicy::Forbid,
                |mut page| {
                    for index in first..end {
                        let entry = (record.lba_ids[index] % ENTRIES_PER_PAGE as u32) as usize;
                        let physical = start + index as u32;
                        remaps[index - first] = (page.entries().physical_addr[entry], physical);
                        page.update(entry, physical, record.checksums[index])
                            .map_err(|error| {
                                io::Error::other(format!("mapping update: {error:?}"))
                            })?;
                    }
                    #[cfg(any(test, feature = "test-failpoints"))]
                    crate::pause_at_test_failpoint(crate::MAPPING_PUBLISHED_FAILPOINT)?;
                    Ok(end - first)
                },
            )?;
            for &(old, new) in &remaps[..applied] {
                self.remap(old, new)?;
            }
            first = end;
        }
        Ok(())
    }
}
