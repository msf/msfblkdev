use std::ffi::OsStr;
use std::io;
use std::path::Path;

pub(super) const ENVIRONMENT_VARIABLE: &str = "BLOCK_STORAGE_IO_LEDGER";

pub(super) fn open_optional(
    path: Option<&OsStr>,
) -> io::Result<Option<block_storage::io_accounting::Writer>> {
    path.map(open_writer).transpose()
}

fn open_writer(path: &OsStr) -> io::Result<block_storage::io_accounting::Writer> {
    // SAFETY: the daemon's normal-user caller owns this private ledger and promises
    // not to truncate or externally mutate it while the child process is live.
    // Writer::open validates the inode type, owner, mode, size, header, and lease.
    unsafe { block_storage::io_accounting::Writer::open(Path::new(path)) }
}
