//! FSYNC-01: a deterministic lost-writeback medium for a tiny private image.
//! Observe byte changes after real kernel completions, without decoding SQE pointers.
use super::BackingIo;
use crate::BLOCK_SIZE;
use io_uring::{IoUring, opcode, squeue};
use std::{fs::File, io, os::unix::fs::FileExt, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Clean,
    BareRetry,
    Engine,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Record {
    offset: usize,
    bytes: Vec<u8>,
}

pub(crate) struct Medium {
    file: File,
    durable: Vec<u8>,
    mode: Mode,
    in_client_flush: bool,
    pub(crate) originals: Vec<Record>,
    pub(crate) replays: Vec<Record>,
    pub(crate) lost: bool,
    pub(crate) synced: bool,
}

impl Medium {
    pub(crate) fn arm(io: &mut BackingIo, path: &Path, mode: Mode) -> io::Result<()> {
        let file = File::options().read(true).write(true).open(path)?;
        // Bound both snapshot memory and rollback to the disposable fixture.
        let len = file.metadata()?.len();
        assert!(file.metadata()?.is_file() && len <= 2 * 1024 * 1024);
        let mut durable = vec![0; len as usize];
        file.read_exact_at(&mut durable, 0)?;
        io.exact.lost_writeback = Some(Box::new(Self {
            file,
            durable,
            mode,
            in_client_flush: false,
            originals: vec![],
            replays: vec![],
            lost: false,
            synced: false,
        }));
        Ok(())
    }

    pub(crate) fn enter_client_flush(io: &mut BackingIo) {
        io.exact
            .lost_writeback
            .as_mut()
            .expect("armed FSYNC-01 medium")
            .in_client_flush = true;
    }

    pub(crate) fn take(io: &mut BackingIo) -> Box<Self> {
        io.exact
            .lost_writeback
            .take()
            .expect("armed FSYNC-01 medium")
    }

    pub(super) fn submit(
        &mut self,
        ring: &mut Option<IoUring>,
        entry: squeue::Entry,
        tag: u64,
        expected: i32,
    ) -> io::Result<()> {
        let code = entry.get_opcode();
        if code == u32::from(opcode::Fsync::CODE) {
            let first = self.barrier(ring, entry.clone(), tag, expected);
            if self.mode == Mode::BareRetry
                && first
                    .as_ref()
                    .is_err_and(|e| e.raw_os_error() == Some(libc::EIO))
                && self.lost
            {
                // Planted policy: swallow the observed writeback error if a bare retry succeeds.
                return self.barrier(ring, entry, tag, expected);
            }
            return first;
        }
        // This workload only appends single-LBA records between barriers. Replays
        // may flatten the iovecs; their physical effects must still be identical.
        assert!(
            !self.synced
                && [opcode::Writev::CODE, opcode::Write::CODE]
                    .map(u32::from)
                    .contains(&code),
            "unexpected FSYNC-01 SQE {code}"
        );
        assert_eq!(
            expected as usize,
            2 * BLOCK_SIZE,
            "expected one payload and one footer"
        );
        let mut before = vec![0; self.durable.len()];
        self.file.read_exact_at(&mut before, 0)?;
        crate::submit_exact_raw(ring, entry, tag, expected)?;
        let mut after = vec![0; before.len()];
        self.file.read_exact_at(&mut after, 0)?;
        let mut changes = before
            .as_chunks::<BLOCK_SIZE>()
            .0
            .iter()
            .zip(after.as_chunks::<BLOCK_SIZE>().0)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i));
        let first = changes
            .next()
            .expect("record must change its payload block");
        assert_eq!(
            changes.next(),
            Some(first + 1),
            "footer must follow payload"
        );
        assert_eq!(changes.next(), None, "extra modified block");
        let offset = first * BLOCK_SIZE;
        let bytes = after[offset..offset + 2 * BLOCK_SIZE].to_vec();
        let records = if self.lost {
            &mut self.replays
        } else {
            &mut self.originals
        };
        assert!(
            records.len() < 2,
            "duplicate or unexpected record submission"
        );
        eprintln!(
            "FSYNC-01 {:?} {} offset={} len={} xxh3={:016x}",
            self.mode,
            if self.lost { "replay" } else { "original" },
            offset,
            bytes.len(),
            xxhash_rust::xxh3::xxh3_64(&bytes)
        );
        records.push(Record { offset, bytes });
        Ok(())
    }

    fn barrier(
        &mut self,
        ring: &mut Option<IoUring>,
        entry: squeue::Entry,
        tag: u64,
        expected: i32,
    ) -> io::Result<()> {
        assert!(
            self.in_client_flush,
            "unexpected barrier before explicit client FLUSH"
        );
        assert_eq!(
            self.originals.len(),
            2,
            "barrier before both acknowledged records"
        );
        assert!(!self.synced, "extra barrier");
        if self.mode != Mode::Clean && !self.lost {
            // Simulate failed writeback: discard unsynced contents, not just the errno.
            // Do not truncate or replace the file; the engine still owns the same inode.
            self.file.write_all_at(&self.durable, 0)?;
            self.file.sync_all()?;
            self.lost = true;
            eprintln!(
                "FSYNC-01 {:?} records barrier: lost both records, EIO",
                self.mode
            );
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        crate::submit_exact_raw(ring, entry, tag, expected)?;
        self.synced = true;
        eprintln!(
            "FSYNC-01 {:?} records barrier: backing sync succeeded",
            self.mode
        );
        Ok(())
    }
}
