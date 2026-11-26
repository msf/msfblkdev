//! Opening a backing: its kind, the ownership claim of each opener, and host allocation.
use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom},
    ops::Range,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

/// The daemon has printed this text since V0.8; the ublk lab compares it exactly.
pub(crate) const BACKING_LOCKED_MESSAGE: &str = "backing file is already locked";
const BACKING_OWNED_MESSAGE: &str = "backing is owned by a live volume";

/// What holds a volume: a regular file or a block device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackingKind {
    RegularFile,
    BlockDevice,
}

/// How an opener shares the backing with other openers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// Reads and writes. Excludes every other `Live` and `Check` opener.
    Live,
    /// Reads only. Excludes `Live` openers, so the backing stays offline while it is checked.
    Check,
    /// Reads only and claims nothing, so it can read a backing that a `Live` opener serves.
    Inspect,
}

/// An open backing and the claim its opener holds.
pub(crate) struct Opened {
    pub(crate) file: File,
    pub(crate) kind: BackingKind,
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
    let exclusive = match (kind, mode) {
        (BackingKind::BlockDevice, OpenMode::Live | OpenMode::Check) => libc::O_EXCL,
        _ => 0,
    };
    let file = OpenOptions::new()
        .read(true)
        .write(mode == OpenMode::Live)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC | exclusive)
        .open(path)
        .map_err(|error| device_busy(error, mode))?;
    // The path can name another file after the first lookup; trust only the open descriptor.
    if kind_of(&file.metadata()?)? != kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "backing changed kind while it was opened",
        ));
    }
    let operation = match (kind, mode) {
        (BackingKind::RegularFile, OpenMode::Live) => libc::LOCK_EX,
        (BackingKind::RegularFile, OpenMode::Check) => libc::LOCK_SH,
        _ => {
            return Ok(Opened {
                file,
                kind,
                claim: Claim(None),
            });
        }
    };
    lock(&file, operation)?;
    let claim = Claim(Some(file.try_clone()?));
    Ok(Opened { file, kind, claim })
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

fn device_busy(error: io::Error, mode: OpenMode) -> io::Error {
    if error.raw_os_error() != Some(libc::EBUSY) {
        return error;
    }
    let message = match mode {
        OpenMode::Check => BACKING_OWNED_MESSAGE,
        OpenMode::Live | OpenMode::Inspect => "backing device is in use",
    };
    io::Error::new(io::ErrorKind::ResourceBusy, message)
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
    let message = if operation == libc::LOCK_EX {
        BACKING_LOCKED_MESSAGE
    } else {
        BACKING_OWNED_MESSAGE
    };
    Err(io::Error::new(io::ErrorKind::WouldBlock, message))
}

/// Size facts about an open backing.
pub(crate) struct Extent {
    /// File length or device size.
    pub(crate) bytes: u64,
    /// `st_blocks` × 512; `None` for a device.
    pub(crate) host_allocated_bytes: Option<u64>,
}

pub(crate) fn extent(mut file: &File, kind: BackingKind) -> io::Result<Extent> {
    let bytes = file.seek(SeekFrom::End(0))?;
    let host_allocated_bytes = match kind {
        BackingKind::RegularFile => Some(file.metadata()?.blocks() * 512),
        BackingKind::BlockDevice => None,
    };
    Ok(Extent {
        bytes,
        host_allocated_bytes,
    })
}

/// Bytes of `range` that hold data in a regular file, from `SEEK_DATA` and `SEEK_HOLE`.
/// Moves the file offset; every engine read and write is positional.
pub(crate) fn allocated_bytes_in(file: &File, range: Range<u64>) -> io::Result<u64> {
    let mut allocated = 0;
    let mut position = range.start;
    // Bounded by the range: each iteration passes one data extent or stops.
    while position < range.end {
        let Some(data) = seek(file, position, libc::SEEK_DATA)? else {
            break;
        };
        if data >= range.end {
            break;
        }
        let hole = seek(file, data, libc::SEEK_HOLE)?.unwrap_or(range.end);
        let end = hole.min(range.end);
        allocated += end - data;
        position = end;
    }
    Ok(allocated)
}

/// `lseek`; `None` when no data follows `offset` (`ENXIO`).
fn seek(file: &File, offset: u64, whence: libc::c_int) -> io::Result<Option<u64>> {
    let offset = libc::off_t::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds off_t"))?;
    // SAFETY: lseek only reads the valid descriptor and changes its file offset.
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if let Ok(position) = u64::try_from(result) {
        return Ok(Some(position));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENXIO) {
        return Ok(None);
    }
    Err(error)
}
