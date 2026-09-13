//! Host-local, opt-in I/O accounting shared through a caller-owned ledger file.
//!
//! The ledger is native-endian and is valid only on the host that created it. A
//! writer reserves the full planned write before the caller submits I/O. Failed
//! or unknown completions keep that reservation and make the exact completion
//! totals permanently incomplete.

#[cfg(not(target_has_atomic = "64"))]
compile_error!("I/O accounting requires native 64-bit atomics");

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: u64 = u64::from_ne_bytes(*b"MBSIOAC1");
const VERSION: u32 = 1;
const MODE: u32 = 0o600;
const FLAG_INCOMPLETE: u64 = 1;
const FLAG_BUDGET_EXHAUSTED: u64 = 2;
const KNOWN_FLAGS: u64 = FLAG_INCOMPLETE | FLAG_BUDGET_EXHAUSTED;
const SNAPSHOT_ATTEMPTS: usize = 100;

#[repr(C, align(8))]
struct Ledger {
    magic: AtomicU64,
    version: u32,
    length: u32,
    owner_uid: u32,
    _reserved: u32,
    write_limit_bytes: u64,
    epoch: AtomicU64,
    write_reserved_bytes: AtomicU64,
    write_completed_bytes: AtomicU64,
    read_completed_bytes: AtomicU64,
    read_completed_operations: AtomicU64,
    write_completed_operations: AtomicU64,
    flush_completed_operations: AtomicU64,
    pending_operations: AtomicU64,
    errors: AtomicU64,
    flags: AtomicU64,
}

const LEDGER_LENGTH: usize = size_of::<Ledger>();

/// A backing operation admitted for later completion.
#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    /// An exact-length backing read.
    Read { bytes: u64 },
    /// A backing write whose full planned length is reserved at admission.
    Write { bytes: u64 },
    /// A backing flush.
    Flush,
}

/// One internally coherent view of the shared counters.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub write_limit_bytes: u64,
    pub write_reserved_bytes: u64,
    pub write_completed_bytes: u64,
    pub read_completed_bytes: u64,
    pub read_completed_operations: u64,
    pub write_completed_operations: u64,
    pub flush_completed_operations: u64,
    pub pending_operations: u64,
    pub errors: u64,
    pub completed_totals_incomplete: bool,
    pub budget_exhausted: bool,
}

struct Mapping {
    address: NonNull<Ledger>,
    file: File,
}

impl Mapping {
    unsafe fn map(file: File, writable: bool) -> io::Result<Self> {
        let protection = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: the caller guarantees that the validated file remains Ledger-sized
        // and is modified only through this module for the mapping's lifetime.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEDGER_LENGTH,
                protection,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let address = NonNull::new(address.cast::<Ledger>())
            .ok_or_else(|| io::Error::other("mmap returned a null address"))?;
        Ok(Self { address, file })
    }

    fn ledger(&self) -> &Ledger {
        // SAFETY: map() established alignment, size, initialization, and the
        // caller-owned-file lifetime invariant for this mapping.
        unsafe { self.address.as_ref() }
    }
}

// SAFETY: the mapping owns its descriptor; mapped shared mutation is atomic and
// writer-side mutation is additionally restricted by the writer lease.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this is the exact live address and length returned by mmap.
        let result = unsafe { libc::munmap(self.address.as_ptr().cast(), LEDGER_LENGTH) };
        debug_assert_eq!(result, 0, "munmap of a live ledger mapping failed");
    }
}

/// Read-only access that may coexist with the single writer.
pub struct Reader {
    mapping: Mapping,
}

impl Reader {
    /// Creates a new mode-0600 ledger and returns a read-only handle.
    ///
    /// # Safety
    ///
    /// Until every ledger handle is dropped, the caller must keep ownership of
    /// the created inode and must not truncate it or modify it except through
    /// this module. Descriptor pinning does not prevent truncation. Violating
    /// this contract can invalidate the typed memory mapping.
    pub unsafe fn create(path: impl AsRef<Path>, write_limit_bytes: u64) -> io::Result<Self> {
        if write_limit_bytes == 0 {
            return Err(invalid_input("write limit must be nonzero"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(MODE)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path.as_ref())?;
        lock_exclusive(&file)?;
        file.set_permissions(std::fs::Permissions::from_mode(MODE))?;
        file.set_len(ledger_length_u64()?)?;
        // SAFETY: this newly created, locked inode satisfies create's contract.
        let mapping = unsafe { Mapping::map(file, true)? };
        // SAFETY: the unpublished mapping is writable and exclusively locked;
        // writing the whole value initializes every byte exactly once.
        unsafe {
            mapping
                .address
                .as_ptr()
                .write(new_ledger(write_limit_bytes)?);
        }
        mapping.ledger().magic.store(MAGIC, Ordering::SeqCst);
        validate_header(mapping.ledger())?;
        unlock(&mapping.file)?;
        let reader_file = mapping.file.try_clone()?;
        drop(mapping);
        // SAFETY: the cloned descriptor names the same caller-owned inode.
        let mapping = unsafe { Mapping::map(reader_file, false)? };
        Ok(Self { mapping })
    }

    /// Opens and validates an existing ledger without taking the writer lease.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the inode is not truncated or modified except
    /// through this module until this handle is dropped. Trusted writers opened
    /// through this module may operate concurrently.
    pub unsafe fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = open_validated(path.as_ref(), false)?;
        // SAFETY: the caller promises the required inode lifetime and mutation rules.
        let mapping = unsafe { Mapping::map(file, false)? };
        validate_header(mapping.ledger())?;
        let reader = Self { mapping };
        reader.snapshot()?;
        Ok(reader)
    }

    /// Reads one coherent snapshot, failing after a bounded number of retries.
    pub fn snapshot(&self) -> io::Result<Snapshot> {
        stable_snapshot(self.mapping.ledger())
    }
}

/// Exclusive accounting writer. It is movable but not cloneable.
pub struct Writer {
    mapping: Mapping,
}

impl Writer {
    /// Opens a clean ledger and takes its nonblocking lifetime writer lease.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the inode is not truncated or modified except
    /// through this module until this writer is dropped.
    pub unsafe fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = open_validated(path.as_ref(), true)?;
        lock_exclusive(&file)?;
        // SAFETY: the caller promises the required inode lifetime and mutation rules.
        let mapping = unsafe { Mapping::map(file, true)? };
        validate_header(mapping.ledger())?;
        let snapshot = stable_snapshot(mapping.ledger())?;
        if snapshot.pending_operations != 0 || snapshot.completed_totals_incomplete {
            return Err(invalid_data("ledger was not closed cleanly"));
        }
        Ok(Self { mapping })
    }

    /// Admits exactly one operation and borrows this writer until completion.
    pub fn begin(&mut self, operation: Operation) -> io::Result<PendingOperation<'_>> {
        if matches!(
            operation,
            Operation::Read { bytes: 0 } | Operation::Write { bytes: 0 }
        ) {
            return Err(invalid_input("I/O length must be nonzero"));
        }
        let ledger = self.mapping.ledger();
        if ledger.pending_operations.load(Ordering::SeqCst) != 0 {
            return Err(io::Error::other("an operation is already pending"));
        }
        let error_after_failure = checked_add(&ledger.errors, 1, "error count overflow")?;
        let epoch = ledger.epoch.load(Ordering::SeqCst);
        if epoch & 1 != 0 || epoch.checked_add(4).is_none() {
            return Err(io::Error::other("invalid or exhausted writer epoch"));
        }
        let reserved = if let Operation::Write { bytes } = &operation {
            let current = ledger.write_reserved_bytes.load(Ordering::SeqCst);
            current
                .checked_add(*bytes)
                .filter(|next| *next <= ledger.write_limit_bytes)
        } else {
            None
        };
        if matches!(operation, Operation::Write { .. }) && reserved.is_none() {
            ledger.epoch.store(epoch + 1, Ordering::SeqCst);
            ledger
                .flags
                .fetch_or(FLAG_BUDGET_EXHAUSTED, Ordering::SeqCst);
            ledger.epoch.store(epoch + 2, Ordering::SeqCst);
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }

        ledger.epoch.store(epoch + 1, Ordering::SeqCst);
        if let Some(bytes) = reserved {
            ledger.write_reserved_bytes.store(bytes, Ordering::SeqCst);
        }
        ledger.pending_operations.store(1, Ordering::SeqCst);
        ledger.epoch.store(epoch + 2, Ordering::SeqCst);
        Ok(PendingOperation {
            writer: self,
            operation,
            finish_epoch: epoch + 4,
            error_after_failure,
            finished: false,
        })
    }
}

/// One admitted operation. Its mutable writer borrow prevents a second admission.
///
/// ```compile_fail
/// # use block_storage::io_accounting::{Operation, Writer};
/// # fn example(writer: &mut Writer) -> std::io::Result<()> {
/// let pending = writer.begin(Operation::Flush)?;
/// let other = writer.begin(Operation::Flush)?; // writer is still borrowed
/// pending.finish(true)?;
/// other.finish(true)
/// # }
/// ```
pub struct PendingOperation<'a> {
    writer: &'a mut Writer,
    operation: Operation,
    finish_epoch: u64,
    error_after_failure: u64,
    finished: bool,
}

impl PendingOperation<'_> {
    /// Finishes the operation. Only exact success contributes completion totals.
    pub fn finish(mut self, success: bool) -> io::Result<()> {
        if !success {
            self.record_failure();
            return Ok(());
        }

        let ledger = self.writer.mapping.ledger();
        let (operation_counter, byte_counter, byte_count) = match &self.operation {
            Operation::Read { bytes } => (
                &ledger.read_completed_operations,
                Some(&ledger.read_completed_bytes),
                *bytes,
            ),
            Operation::Write { bytes } => (
                &ledger.write_completed_operations,
                Some(&ledger.write_completed_bytes),
                *bytes,
            ),
            Operation::Flush => (&ledger.flush_completed_operations, None, 0),
        };
        let operations = checked_add(operation_counter, 1, "completed operation count overflow")?;
        let completed_bytes = byte_counter
            .map(|counter| checked_add(counter, byte_count, "completed byte count overflow"))
            .transpose()?;

        begin_reserved_finish(ledger, self.finish_epoch);
        operation_counter.store(operations, Ordering::SeqCst);
        if let (Some(counter), Some(bytes)) = (byte_counter, completed_bytes) {
            counter.store(bytes, Ordering::SeqCst);
        }
        ledger.pending_operations.store(0, Ordering::SeqCst);
        ledger.epoch.store(self.finish_epoch, Ordering::SeqCst);
        self.finished = true;
        Ok(())
    }

    fn record_failure(&mut self) {
        let ledger = self.writer.mapping.ledger();
        begin_reserved_finish(ledger, self.finish_epoch);
        ledger.pending_operations.store(0, Ordering::SeqCst);
        ledger
            .errors
            .store(self.error_after_failure, Ordering::SeqCst);
        ledger.flags.fetch_or(FLAG_INCOMPLETE, Ordering::SeqCst);
        ledger.epoch.store(self.finish_epoch, Ordering::SeqCst);
        self.finished = true;
    }
}

impl Drop for PendingOperation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // begin reserved both the epoch transition and error increment, so
            // abandonment can be recorded without a fallible operation in Drop.
            self.record_failure();
        }
    }
}

fn new_ledger(write_limit_bytes: u64) -> io::Result<Ledger> {
    Ok(Ledger {
        magic: AtomicU64::new(0),
        version: VERSION,
        length: u32::try_from(LEDGER_LENGTH).map_err(|_| invalid_input("ledger too large"))?,
        owner_uid: effective_uid(),
        _reserved: 0,
        write_limit_bytes,
        epoch: AtomicU64::new(0),
        write_reserved_bytes: AtomicU64::new(0),
        write_completed_bytes: AtomicU64::new(0),
        read_completed_bytes: AtomicU64::new(0),
        read_completed_operations: AtomicU64::new(0),
        write_completed_operations: AtomicU64::new(0),
        flush_completed_operations: AtomicU64::new(0),
        pending_operations: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        flags: AtomicU64::new(0),
    })
}

fn begin_reserved_finish(ledger: &Ledger, finish_epoch: u64) {
    debug_assert_eq!(
        ledger.epoch.load(Ordering::SeqCst),
        finish_epoch - 2,
        "pending guard must own the next epoch transition"
    );
    ledger.epoch.store(finish_epoch - 1, Ordering::SeqCst);
}

fn checked_add(value: &AtomicU64, amount: u64, message: &'static str) -> io::Result<u64> {
    value
        .load(Ordering::SeqCst)
        .checked_add(amount)
        .ok_or_else(|| io::Error::other(message))
}

fn stable_snapshot(ledger: &Ledger) -> io::Result<Snapshot> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let before = ledger.epoch.load(Ordering::SeqCst);
        if before & 1 != 0 {
            std::hint::spin_loop();
            continue;
        }
        let flags = ledger.flags.load(Ordering::SeqCst);
        let snapshot = Snapshot {
            write_limit_bytes: ledger.write_limit_bytes,
            write_reserved_bytes: ledger.write_reserved_bytes.load(Ordering::SeqCst),
            write_completed_bytes: ledger.write_completed_bytes.load(Ordering::SeqCst),
            read_completed_bytes: ledger.read_completed_bytes.load(Ordering::SeqCst),
            read_completed_operations: ledger.read_completed_operations.load(Ordering::SeqCst),
            write_completed_operations: ledger.write_completed_operations.load(Ordering::SeqCst),
            flush_completed_operations: ledger.flush_completed_operations.load(Ordering::SeqCst),
            pending_operations: ledger.pending_operations.load(Ordering::SeqCst),
            errors: ledger.errors.load(Ordering::SeqCst),
            completed_totals_incomplete: flags & FLAG_INCOMPLETE != 0,
            budget_exhausted: flags & FLAG_BUDGET_EXHAUSTED != 0,
        };
        if before == ledger.epoch.load(Ordering::SeqCst) {
            if flags & !KNOWN_FLAGS != 0
                || snapshot.write_reserved_bytes > snapshot.write_limit_bytes
                || snapshot.write_completed_bytes > snapshot.write_reserved_bytes
                || snapshot.pending_operations > 1
            {
                return Err(invalid_data("invalid ledger counters or flags"));
            }
            return Ok(snapshot);
        }
        std::hint::spin_loop();
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "ledger changed throughout bounded snapshot retries",
    ))
}

fn open_validated(path: &Path, writable: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(writable)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid_data("ledger is not a regular file"));
    }
    if metadata.uid() != effective_uid() || metadata.permissions().mode() & 0o7777 != MODE {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ledger owner or mode mismatch",
        ));
    }
    if metadata.len() != ledger_length_u64()? {
        return Err(invalid_data("ledger size mismatch"));
    }
    Ok(file)
}

fn validate_header(ledger: &Ledger) -> io::Result<()> {
    if ledger.magic.load(Ordering::SeqCst) != MAGIC
        || ledger.version != VERSION
        || usize::try_from(ledger.length) != Ok(LEDGER_LENGTH)
        || ledger.owner_uid != effective_uid()
        || ledger.write_limit_bytes == 0
    {
        return Err(invalid_data("ledger header mismatch"));
    }
    Ok(())
}

fn ledger_length_u64() -> io::Result<u64> {
    u64::try_from(LEDGER_LENGTH).map_err(|_| invalid_data("ledger too large"))
}

fn lock_exclusive(file: &File) -> io::Result<()> {
    // SAFETY: flock uses this live descriptor and valid lock flags.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlock(file: &File) -> io::Result<()> {
    // SAFETY: flock uses this live descriptor and the valid unlock operation.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
