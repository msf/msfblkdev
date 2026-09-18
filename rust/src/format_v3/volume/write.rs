use super::*;
use crate::format_v3::{
    RecordHeader, RecordKind,
    buffered_pages::{BufferedPages, PageGroup},
    checkpoint_publish::LogPosition,
    data_record::DataRecord,
    geometry::Geometry,
    reservation::{Reservation, ensure_space},
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
        ensure_space(
            self.mapping.tail.boundary,
            self.mapping.checkpoint.descriptor.backing_blocks,
            reservation.total_blocks,
        )?;
        assert!(self.write_buffer.store(lba, data), "new buffered LBA");
        self.buffered_pages = reservation.data_records as usize;
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
        let mut cursor = 0;
        while cursor < self.write_buffer.len() {
            let Some(group) = BufferedPages::new(&self.write_buffer, cursor).next() else {
                break;
            };
            cursor = group.slots()[0] + 1;
            let result = self.write_group(&group);
            self.latch(result)?;
        }
        self.write_buffer.clear();
        self.buffered_pages = 0;
        Ok(())
    }

    fn write_group(&mut self, group: &PageGroup) -> io::Result<()> {
        let count = group.slots().len();
        let record_bytes = (count + 1) * BLOCK_SIZE;
        let since_checkpoint = u64::from(
            self.mapping.tail.boundary - self.mapping.checkpoint.descriptor.last_record_boundary,
        ) * BLOCK_SIZE as u64;
        if since_checkpoint + record_bytes as u64 > self.checkpoint_after_bytes {
            self.mapping.publish(&mut self.backing_io, &self.backing)?;
        }
        let buffer = &self.write_buffer;
        let backing = &self.backing;
        let mut writer = RecordWriter {
            backing,
            footer: &mut self.footer_scratch,
            iovecs: &mut self.release_iovecs.0,
            geometry: self.mapping.checkpoint.descriptor.geometry()?,
            volume_id: self.mapping.checkpoint.descriptor.volume_id,
        };
        let (next, newly_mapped) = self.mapping.with_page(
            &mut self.backing_io,
            backing,
            group.page_id,
            |mut page, tail, backing_io| {
                let record = writer.append(backing_io, buffer, group, tail)?;
                let mut newly_mapped = 0;
                for index in 0..count {
                    let entry = (record.lba_ids[index] % ENTRIES_PER_PAGE as u32) as usize;
                    newly_mapped += u64::from(page.entries().physical_addr[entry] == 0);
                    page.update(
                        entry,
                        tail.boundary + 1 + index as u32,
                        record.checksums[index],
                    )
                    .map_err(|error| io::Error::other(format!("mapping update: {error:?}")))?;
                }
                #[cfg(any(test, feature = "test-failpoints"))]
                crate::pause_at_test_failpoint(crate::MAPPING_PUBLISHED_FAILPOINT)?;
                Ok((
                    LogPosition {
                        lsn: record.header.lsn,
                        boundary: record.header.boundary,
                    },
                    newly_mapped,
                ))
            },
        )?;
        self.mapping.tail = next;
        self.live_blocks += newly_mapped;
        self.release_stats.record(count);
        Ok(())
    }
}

struct RecordWriter<'a> {
    backing: &'a File,
    footer: &'a mut AlignedBlock,
    iovecs: &'a mut Vec<libc::iovec>,
    geometry: Geometry,
    volume_id: u64,
}

impl RecordWriter<'_> {
    fn append(
        &mut self,
        backing_io: &mut BackingIo,
        buffer: &WriteBackBuffer,
        group: &PageGroup,
        tail: LogPosition,
    ) -> io::Result<DataRecord> {
        let count = group.slots().len();
        let boundary = tail
            .boundary
            .checked_add(count as u32 + 1)
            .ok_or_else(|| io::Error::other("log address exhausted"))?;
        assert!(
            u64::from(boundary) < self.geometry.backing_blocks,
            "admitted record has reserved log space"
        );
        let header = RecordHeader {
            kind: RecordKind::Data,
            volume_id: self.volume_id,
            lsn: tail
                .lsn
                .checked_add(1)
                .ok_or_else(|| io::Error::other("record LSN exhausted"))?,
            previous_boundary: tail.boundary,
            boundary,
        };
        let record = data_record(buffer, group, header);
        record.encode(self.geometry, &mut self.footer.0)?;
        self.iovecs.clear();
        for slot in group.slots() {
            self.iovecs.push(libc::iovec {
                iov_base: buffer.arena[*slot].0.as_ptr() as *mut libc::c_void,
                iov_len: BLOCK_SIZE,
            });
        }
        self.iovecs.push(libc::iovec {
            iov_base: self.footer.0.as_ptr() as *mut libc::c_void,
            iov_len: BLOCK_SIZE,
        });
        let write = opcode::Writev::new(
            types::Fd(self.backing.as_raw_fd()),
            self.iovecs.as_ptr(),
            self.iovecs.len() as u32,
        )
        .offset((u64::from(tail.boundary) + 1) * BLOCK_SIZE as u64)
        .build();
        submit_exact(
            backing_io,
            write,
            header.lsn,
            ((count + 1) * BLOCK_SIZE) as i32,
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::RECORD_WRITE_COMPLETE_FAILPOINT)?;
        Ok(record)
    }
}

fn data_record(buffer: &WriteBackBuffer, group: &PageGroup, header: RecordHeader) -> DataRecord {
    let mut record = DataRecord {
        header,
        lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
        checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
    };
    for (index, slot) in group.slots().iter().enumerate() {
        let lba = buffer.lbas[*slot];
        record.lba_ids[index] = lba;
        record.checksums[index] = payload_checksum(
            header.volume_id,
            lba,
            header.previous_boundary + 1 + index as u32,
            &buffer.arena[*slot].0,
        );
    }
    record
}
