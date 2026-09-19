use crate::io_accounting::{Operation, Writer};
use io_uring::{IoUring, opcode, squeue};
use std::io;

pub(super) struct BackingIo {
    pub(super) ring: Option<IoUring>,
    accounting: Option<Writer>,
    pub(super) mapping_io: crate::format_v3::metrics::MappingIo,
    #[cfg(test)]
    pub(super) metadata_short_write: Option<(u32, usize)>,
    #[cfg(test)]
    pub(super) metadata_short_read: Option<usize>,
}

impl BackingIo {
    pub(super) fn new(accounting: Option<Writer>) -> io::Result<Self> {
        Ok(Self {
            ring: Some(IoUring::new(2)?),
            accounting,
            mapping_io: crate::format_v3::metrics::MappingIo::default(),
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
    let BackingIo {
        ring, accounting, ..
    } = backing_io;
    let Some(writer) = accounting.as_mut() else {
        return super::submit_exact_raw(ring, entry, user_data, expected_result);
    };
    let operation = match classify_operation(&entry, expected_result) {
        Ok(operation) => operation,
        Err(error) => return reject_unclassified(writer, error),
    };
    let pending = writer.begin(operation)?;
    let io_result = super::submit_exact_raw(ring, entry, user_data, expected_result);
    let accounting_result = pending.finish(io_result.is_ok());
    match io_result {
        Err(error) => Err(error),
        Ok(()) => accounting_result,
    }
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
