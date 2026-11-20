use crate::io_accounting::{Operation, Writer};
use io_uring::{IoUring, opcode, squeue};
use std::io;

#[cfg(test)]
pub(crate) mod fault_plan;
#[cfg(test)]
pub(crate) mod fsync01;

struct ExactIo {
    ring: Option<IoUring>,
    #[cfg(test)]
    plan: Option<fault_plan::FaultPlan>,
    #[cfg(test)]
    lost_writeback: Option<Box<fsync01::Medium>>,
}

impl ExactIo {
    fn submit(&mut self, entry: squeue::Entry, tag: u64, expected: i32) -> io::Result<()> {
        #[cfg(test)]
        if let Some(medium) = &mut self.lost_writeback {
            return medium.submit(&mut self.ring, entry, tag, expected);
        }
        #[cfg(test)]
        if let Some(action) = self.plan.as_mut().and_then(|plan| plan.hit(&entry)) {
            return action.submit(&mut self.ring, entry, tag, expected);
        }
        super::submit_exact_raw(&mut self.ring, entry, tag, expected)
    }
}

pub(super) struct BackingIo {
    exact: ExactIo,
    accounting: Option<Writer>,
    pub(super) mapping_io: crate::format_v4::metrics::MappingIo,
    #[cfg(test)]
    pub(super) metadata_short_write: Option<(u32, usize)>,
    #[cfg(test)]
    pub(super) metadata_short_read: Option<usize>,
}

impl BackingIo {
    pub(super) fn new(accounting: Option<Writer>) -> io::Result<Self> {
        Ok(Self {
            exact: ExactIo {
                ring: Some(IoUring::new(2)?),
                #[cfg(test)]
                plan: None,
                #[cfg(test)]
                lost_writeback: None,
            },
            accounting,
            mapping_io: crate::format_v4::metrics::MappingIo::default(),
            #[cfg(test)]
            metadata_short_write: None,
            #[cfg(test)]
            metadata_short_read: None,
        })
    }
}

pub(super) fn submit_exact(
    backing_io: &mut BackingIo,
    entry: squeue::Entry,
    user_data: u64,
    expected_result: i32,
) -> io::Result<()> {
    let Some(writer) = backing_io.accounting.as_mut() else {
        return backing_io.exact.submit(entry, user_data, expected_result);
    };
    let operation = match classify_operation(&entry, expected_result) {
        Ok(operation) => operation,
        Err(error) => return reject_unclassified(writer, error),
    };
    let pending = writer.begin(operation)?;
    let io_result = backing_io.exact.submit(entry, user_data, expected_result);
    let accounting_result = pending.finish(io_result.is_ok());
    match io_result {
        Err(error) => Err(error),
        Ok(()) => accounting_result,
    }
}

/// Submits an operation the ledger does not classify, such as a hole punch.
pub(super) fn submit_exact_unaccounted(
    backing_io: &mut BackingIo,
    entry: squeue::Entry,
    user_data: u64,
    expected_result: i32,
) -> io::Result<()> {
    backing_io.exact.submit(entry, user_data, expected_result)
}

fn classify_operation(entry: &squeue::Entry, expected_result: i32) -> io::Result<Operation> {
    let code = entry.get_opcode();
    if code == u32::from(opcode::Read::CODE) {
        return Ok(Operation::Read {
            bytes: positive_bytes(expected_result)?,
        });
    }
    if code == u32::from(opcode::Write::CODE) || code == u32::from(opcode::Writev::CODE) {
        return Ok(Operation::Write {
            bytes: positive_bytes(expected_result)?,
        });
    }
    if code != u32::from(opcode::Fsync::CODE) {
        return Err(invalid_input("unsupported accounted io_uring opcode"));
    }
    if expected_result != 0 {
        return Err(invalid_input("fsync expected result must be zero"));
    }
    Ok(Operation::Flush)
}

fn positive_bytes(expected_result: i32) -> io::Result<u64> {
    u64::try_from(expected_result)
        .ok()
        .filter(|bytes| *bytes != 0)
        .ok_or_else(|| invalid_input("I/O expected result must be positive"))
}

fn reject_unclassified(writer: &mut Writer, error: io::Error) -> io::Result<()> {
    writer.begin(Operation::Flush)?.finish(false)?;
    Err(error)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
