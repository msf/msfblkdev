//! Single-shot rules for one test-owned `BackingIo`. No process-global state.
use super::BackingIo;
use io_uring::{IoUring, opcode, squeue};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Action {
    /// Reject before submitting the original SQE. The backing does not change.
    Reject(i32),
    /// Model a zero-byte read/write. A safe NOP supplies a real zero-result CQE;
    /// the original operation (and all its pointers) never enters the kernel.
    ShortZero,
    /// Complete the original request and reap its CQE before reporting an error.
    ErrorAfterSuccess(i32),
}

impl Action {
    pub(super) fn submit(
        self,
        ring: &mut Option<IoUring>,
        entry: squeue::Entry,
        tag: u64,
        expected: i32,
    ) -> io::Result<()> {
        match self {
            Self::Reject(errno) => Err(io::Error::from_raw_os_error(errno)),
            Self::ShortZero => {
                assert!(expected > 0, "short rule requires a byte transfer");
                crate::submit_exact_raw(ring, opcode::Nop::new().build(), tag, expected)
            }
            Self::ErrorAfterSuccess(errno) => {
                crate::submit_exact_raw(ring, entry, tag, expected).map_err(|error| {
                    io::Error::other(format!(
                        "real I/O failed before the injected error: {error}"
                    ))
                })?;
                Err(io::Error::from_raw_os_error(errno))
            }
        }
    }
}

pub(crate) struct FaultPlan {
    opcode: u8,
    remaining: usize,
    action: Action,
    hits: Arc<AtomicUsize>,
}

impl FaultPlan {
    /// Match the one-based occurrence of this opcode after installation. Keep the
    /// counter outside the formatter/opener so errors that consume the ring are observable.
    pub(crate) fn install(
        io: &mut BackingIo,
        opcode: u8,
        occurrence: usize,
        action: Action,
    ) -> Arc<AtomicUsize> {
        assert!(occurrence > 0, "fault occurrence is one-based");
        match action {
            Action::Reject(errno) | Action::ErrorAfterSuccess(errno) => assert!(errno > 0),
            Action::ShortZero => assert!(matches!(
                opcode,
                opcode::Read::CODE | opcode::Write::CODE | opcode::Writev::CODE
            )),
        }
        let hits = Arc::new(AtomicUsize::new(0));
        io.exact.plan = Some(Self {
            opcode,
            remaining: occurrence,
            action,
            hits: Arc::clone(&hits),
        });
        hits
    }

    pub(super) fn hit(&mut self, entry: &squeue::Entry) -> Option<Action> {
        if self.remaining == 0 || entry.get_opcode() != u32::from(self.opcode) {
            return None;
        }
        self.remaining -= 1;
        if self.remaining != 0 {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(self.action)
    }
}

mod adr05_seam;
