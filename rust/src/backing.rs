//! Opening a backing: its kind, the ownership claim of each opener, and host allocation.
use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, OpenOptionsExt},
    },
    path::Path,
};

/// The daemon has printed this text since V0.8; the ublk lab compares it exactly.
pub(crate) const BACKING_LOCKED_MESSAGE: &str = "backing file is already locked";

/// What holds a volume: a regular file or a block device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackingKind {
    RegularFile,
    BlockDevice,
}

/// How an opener shares the backing with other openers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// Reads and writes. Excludes every other `Live` opener.
    Live,
}

/// An open backing and the claim its opener holds.
pub(crate) struct Opened {
    pub(crate) file: File,
    pub(crate) claim: Claim,
}

/// Holds a regular file's `flock` and releases it explicitly when dropped.
///
/// `flock` attaches to the open file description. A child that another thread forks keeps a
/// reference to it until it execs, so closing our descriptor alone can leave the lock held for
/// a moment after the owner is gone. `LOCK_UN` releases it at once. A device's `O_EXCL` claim
/// ends when its descriptor closes.
pub(crate) struct Claim(Option<File>);

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            // SAFETY: flock only reads the valid descriptor and the operation flags.
            // A failed unlock needs no handling: closing the descriptor releases the lock too.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Opens `path` with `O_DIRECT` and takes the claim that `mode` requires.
///
/// A regular file is claimed with `flock`, which follows the inode. A block device is claimed
/// with `O_EXCL`, which the kernel enforces for every path to the device and against mounts.
pub(crate) fn open(path: &Path, mode: OpenMode) -> io::Result<Opened> {
    let kind = kind_of(&std::fs::metadata(path)?)?;
    let exclusive = match kind {
        BackingKind::BlockDevice => libc::O_EXCL,
        BackingKind::RegularFile => 0,
    };
    let file = OpenOptions::new()
        .read(true)
        .write(mode == OpenMode::Live)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC | exclusive)
        .open(path)
        .map_err(device_busy)?;
    // The path can name another file after the first lookup; trust only the open descriptor.
    if kind_of(&file.metadata()?)? != kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "backing changed kind while it was opened",
        ));
    }
    if kind == BackingKind::BlockDevice {
        return Ok(Opened {
            file,
            claim: Claim(None),
        });
    }
    lock(&file, libc::LOCK_EX)?;
    let claim = Claim(Some(file.try_clone()?));
    Ok(Opened { file, claim })
}

fn kind_of(metadata: &std::fs::Metadata) -> io::Result<BackingKind> {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        return Ok(BackingKind::RegularFile);
    }
    if file_type.is_block_device() {
        return Ok(BackingKind::BlockDevice);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "backing must be a regular file or a block device",
    ))
}

fn device_busy(error: io::Error) -> io::Error {
    if error.raw_os_error() != Some(libc::EBUSY) {
        return error;
    }
    io::Error::new(io::ErrorKind::ResourceBusy, "backing device is in use")
}

fn lock(file: &File, operation: libc::c_int) -> io::Result<()> {
    // SAFETY: flock only reads the valid descriptor and the operation flags.
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::WouldBlock {
        return Err(error);
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        BACKING_LOCKED_MESSAGE,
    ))
}
