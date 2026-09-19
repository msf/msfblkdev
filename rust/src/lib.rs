#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

mod accounted_io;
mod format_v3;
mod mapping_cache;

pub mod io_accounting;
pub mod logging;

use accounted_io::{BackingIo, submit_exact};
pub use format_v3::metrics::{MappingIo, MappingStats};
pub use format_v3::volume::Volume;
pub use mapping_cache::sizing::Sizing as CacheSizing;
pub use mapping_cache::{Counters as CacheCounters, Lookups, Snapshot as CacheSnapshot};

const DEFAULT_CHECKPOINT_AFTER_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(any(test, feature = "test-failpoints"))]
const CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT: &str = "checkpoint-body-fsync-complete";
#[cfg(any(test, feature = "test-failpoints"))]
const DESCRIPTOR_WRITE_COMPLETE_FAILPOINT: &str = "descriptor-write-complete";
#[cfg(any(test, feature = "test-failpoints"))]
const DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT: &str = "descriptor-fsync-complete";

const BLOCK_SIZE: usize = 4096;
const MAX_VOLUME_BLOCKS: u64 = 1 << 31;
const MAX_BACKING_BLOCKS: u64 = u32::MAX as u64 + 1;
const MAXIMUM_RECORD_BLOCKS: u64 = 339;
const MAXIMUM_PAYLOAD_BLOCKS: usize = MAXIMUM_RECORD_BLOCKS as usize - 1;
// Library default: a one-block window makes every write its own record (write-through).
// The ublk daemon opts into a larger write-back buffer through VolumeOpenOptions.
const MINIMUM_WRITE_BUFFER_BYTES: u64 = BLOCK_SIZE as u64;
// A release writes the whole buffer and then fsyncs it; a larger buffer turns one FLUSH into an
// unacceptably long pause. The plan caps the write-back buffer at 16 MiB.
const MAXIMUM_WRITE_BUFFER_BYTES: u64 = 16 * 1024 * 1024;
// Limit buffer allocation to a quarter of the log. Admission separately reserves
// data records and mapping snapshots, including intermediate pressure checkpoints.
const WRITE_BUFFER_DEVICE_DIVISOR: u64 = 4;

#[cfg(any(test, feature = "test-failpoints"))]
const TEST_FAILPOINT_ENV: &str = "BLOCK_STORAGE_TEST_FAILPOINT";
#[cfg(any(test, feature = "test-failpoints"))]
const RECORD_WRITE_COMPLETE_FAILPOINT: &str = "record-write-complete";
#[cfg(any(test, feature = "test-failpoints"))]
const MAPPING_PUBLISHED_FAILPOINT: &str = "mapping-published";
#[cfg(any(test, feature = "test-failpoints"))]
const LOG_FSYNC_COMPLETE_FAILPOINT: &str = "log-fsync-complete";
#[cfg(any(test, feature = "test-failpoints"))]
const STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT: &str = "stale-tail-clear-block-complete";
#[cfg(any(test, feature = "test-failpoints"))]
const STALE_TAIL_FSYNC_COMPLETE_FAILPOINT: &str = "stale-tail-fsync-complete";

#[cfg(any(test, feature = "test-failpoints"))]
fn pause_at_test_failpoint(name: &str) -> io::Result<()> {
    if std::env::var(TEST_FAILPOINT_ENV).as_deref() != Ok(name) {
        return Ok(());
    }

    println!("{name}");
    let mut stdout = io::stdout();
    std::io::Write::flush(&mut stdout)?;
    loop {
        std::thread::park();
    }
}

#[repr(align(4096))]
struct AlignedBlock([u8; BLOCK_SIZE]);

/// Preallocated `writev` iovecs reused per release. It only holds pointers into this `Volume`'s own
/// stable heap arena and footer scratch, and is cleared between releases.
struct ReleaseIovecs(Vec<libc::iovec>);

// SAFETY: the iovecs reference arena and footer memory owned by the same `Volume`. That memory is
// heap-allocated and stable for the volume's life, so it stays valid when the `Volume` moves
// between threads, and the vector is empty except during a single-threaded release call.
unsafe impl Send for ReleaseIovecs {}

/// Reason a write-back release was triggered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseTrigger {
    /// The buffer reached capacity while accepting a write.
    Full,
    /// An explicit flush, FUA, close, or checkpoint drained the buffer.
    Flush,
}

/// Opt-in, in-memory batching counters. The payloads-per-record distribution and the
/// footer/payload byte ratio are the primary evidence that write-back batching works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReleaseStats {
    /// Records emitted, one footer each.
    pub records: u64,
    /// Payloads written across all records.
    pub payloads: u64,
    /// Footer bytes written (one 4 KiB footer per record).
    pub footer_bytes: u64,
    /// Payload bytes written.
    pub payload_bytes: u64,
    /// Releases triggered by a full buffer.
    pub full_releases: u64,
    /// Releases triggered by flush, FUA, close, or checkpoint.
    pub flush_releases: u64,
    /// Payloads-per-record histogram: [1], [2], [3-8], [9-32], [33-128], [129-337], [338].
    pub payload_histogram: [u64; 7],
}

impl ReleaseStats {
    fn record(&mut self, payloads: usize) {
        self.records += 1;
        self.payloads += payloads as u64;
        self.footer_bytes += BLOCK_SIZE as u64;
        self.payload_bytes += payloads as u64 * BLOCK_SIZE as u64;
        let bucket = match payloads {
            0 | 1 => 0,
            2 => 1,
            3..=8 => 2,
            9..=32 => 3,
            33..=128 => 4,
            129..=337 => 5,
            _ => 6,
        };
        self.payload_histogram[bucket] += 1;
    }

    /// A single-line summary suitable for emission outside timed work.
    #[must_use]
    pub fn summary(&self) -> String {
        let ratio = if self.payload_bytes == 0 {
            0.0
        } else {
            (self.footer_bytes + self.payload_bytes) as f64 / self.payload_bytes as f64
        };
        format!(
            "records={} payloads={} payloads_per_record={:.3} backing_over_payload={:.3} footer_bytes={} payload_bytes={} full_releases={} flush_releases={} histogram={:?}",
            self.records,
            self.payloads,
            if self.records == 0 {
                0.0
            } else {
                self.payloads as f64 / self.records as f64
            },
            ratio,
            self.footer_bytes,
            self.payload_bytes,
            self.full_releases,
            self.flush_releases,
            self.payload_histogram,
        )
    }
}

/// Physical and logical space accounting for one volume.
///
/// The backing file is sparse, so its declared length is a ceiling rather than
/// a cost. `allocated_bytes` is what the host filesystem has really given the
/// file, and it grows as the log is appended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceUsage {
    /// Addressable logical capacity presented to the filesystem.
    pub volume_bytes: u64,
    /// Logical bytes that hold data, meaning blocks with a mapping.
    pub live_bytes: u64,
    /// Declared backing length. Sparse, so mostly unallocated.
    pub backing_bytes: u64,
    /// Backing bytes the host filesystem has actually allocated.
    pub allocated_bytes: u64,
    /// Both descriptors and index bodies. Appended mapping snapshots count as log use.
    pub metadata_bytes: u64,
    /// Log region length, which bounds writes before `ENOSPC`.
    pub log_capacity_bytes: u64,
    /// Log region consumed by records so far.
    pub log_used_bytes: u64,
}

impl SpaceUsage {
    /// Log bytes still available before `ENOSPC`.
    #[must_use]
    pub const fn log_free_bytes(&self) -> u64 {
        self.log_capacity_bytes.saturating_sub(self.log_used_bytes)
    }

    /// Allocated backing bytes per live logical byte. Zero when nothing is live.
    #[must_use]
    pub fn amplification(&self) -> f64 {
        if self.live_bytes == 0 {
            return 0.0;
        }
        self.allocated_bytes as f64 / self.live_bytes as f64
    }

    /// A single-line summary suitable for emission outside timed work.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "volume_mib={:.1} live_mib={:.1} backing_declared_mib={:.1} backing_allocated_mib={:.1} metadata_mib={:.1} log_used_mib={:.1} log_free_mib={:.1} log_capacity_mib={:.1} log_used_percent={:.1} allocated_per_live={:.3}",
            mib(self.volume_bytes),
            mib(self.live_bytes),
            mib(self.backing_bytes),
            mib(self.allocated_bytes),
            mib(self.metadata_bytes),
            mib(self.log_used_bytes),
            mib(self.log_free_bytes()),
            mib(self.log_capacity_bytes),
            if self.log_capacity_bytes == 0 {
                0.0
            } else {
                100.0 * self.log_used_bytes as f64 / self.log_capacity_bytes as f64
            },
            self.amplification(),
        )
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Configuration for opening a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeOpenOptions {
    /// Maximum physical log bytes retained after the selected checkpoint.
    pub checkpoint_after_bytes: u64,
    /// Write-back buffer capacity in bytes, clamped to the device at open. A one-block value keeps
    /// write-through semantics; larger values coalesce writes and release them on flush or FUA.
    pub write_buffer_bytes: u64,
    /// Mapping-cache allocation budget; None uses the larger of logical bytes / 10,000
    /// and the minimum needed by the effective write buffer.
    /// Must hold min(effective buffer slots, volume mapping pages) pages.
    pub cache_budget_bytes: Option<u64>,
}

impl Default for VolumeOpenOptions {
    fn default() -> Self {
        Self {
            checkpoint_after_bytes: DEFAULT_CHECKPOINT_AFTER_BYTES,
            write_buffer_bytes: MINIMUM_WRITE_BUFFER_BYTES,
            cache_budget_bytes: None,
        }
    }
}

impl VolumeOpenOptions {
    fn validate(self) -> io::Result<()> {
        if !self
            .checkpoint_after_bytes
            .is_multiple_of(BLOCK_SIZE as u64)
            || self.checkpoint_after_bytes < MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64
        {
            return Err(invalid_input("invalid checkpoint_after_bytes"));
        }
        if !self.write_buffer_bytes.is_multiple_of(BLOCK_SIZE as u64)
            || self.write_buffer_bytes < MINIMUM_WRITE_BUFFER_BYTES
            || self.write_buffer_bytes > MAXIMUM_WRITE_BUFFER_BYTES
        {
            return Err(invalid_input("invalid write_buffer_bytes"));
        }
        Ok(())
    }
}

/// A preallocated, 4 KiB-aligned write-back buffer indexed by logical block address.
struct WriteBackBuffer {
    arena: Vec<AlignedBlock>,
    lbas: Vec<u32>,
    slot_of_lba: std::collections::HashMap<u32, usize>,
}

impl WriteBackBuffer {
    fn with_capacity(capacity_blocks: usize) -> Self {
        assert!(
            capacity_blocks >= 1,
            "write buffer needs at least one block"
        );
        let mut arena = Vec::new();
        arena.reserve_exact(capacity_blocks);
        arena.resize_with(capacity_blocks, || AlignedBlock([0; BLOCK_SIZE]));
        let mut lbas = Vec::new();
        lbas.reserve_exact(capacity_blocks);
        let mut slot_of_lba = std::collections::HashMap::with_capacity(capacity_blocks);
        slot_of_lba.reserve(capacity_blocks);
        Self {
            arena,
            lbas,
            slot_of_lba,
        }
    }

    fn capacity(&self) -> usize {
        self.arena.len()
    }

    fn len(&self) -> usize {
        self.lbas.len()
    }

    fn get(&self, lba: u32) -> Option<&[u8; BLOCK_SIZE]> {
        self.slot_of_lba.get(&lba).map(|slot| &self.arena[*slot].0)
    }

    /// Overwrites the buffered payload for `lba` in place, or appends a new slot. Returns true when
    /// the payload occupies a new slot, i.e. when it reserves fresh log space.
    fn store(&mut self, lba: u32, data: &[u8; BLOCK_SIZE]) -> bool {
        if let Some(slot) = self.slot_of_lba.get(&lba) {
            self.arena[*slot].0.copy_from_slice(data);
            return false;
        }
        let slot = self.lbas.len();
        assert!(slot < self.arena.len(), "write buffer overflow");
        self.arena[slot].0.copy_from_slice(data);
        self.lbas.push(lba);
        self.slot_of_lba.insert(lba, slot);
        true
    }

    fn clear(&mut self) {
        self.lbas.clear();
        self.slot_of_lba.clear();
    }

    fn sort_by_lba(&mut self) {
        // Payloads stay in their arena slots; the lookup retains their locations.
        self.lbas.sort_unstable();
    }
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn random_volume_id() -> io::Result<u64> {
    let mut bytes = [0; size_of::<u64>()];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn payload_checksum(volume_id: u64, lba: u32, physical_block: u32, payload: &[u8]) -> u64 {
    let mut hasher = Xxh3::with_seed(volume_id);
    hasher.update(&lba.to_le_bytes());
    hasher.update(&physical_block.to_le_bytes());
    hasher.update(payload);
    hasher.digest()
}

fn read_block_exact(
    backing_io: &mut BackingIo,
    backing: &File,
    physical_block: u32,
    block: &mut AlignedBlock,
) -> io::Result<()> {
    let read = opcode::Read::new(
        types::Fd(backing.as_raw_fd()),
        block.0.as_mut_ptr(),
        BLOCK_SIZE as u32,
    )
    .offset(u64::from(physical_block) * BLOCK_SIZE as u64)
    .build();
    submit_exact(
        backing_io,
        read,
        u64::from(physical_block),
        BLOCK_SIZE as i32,
    )
}

fn clear_stale_tail(
    backing_io: &mut BackingIo,
    backing: &File,
    append_block: u64,
    backing_blocks: u64,
) -> io::Result<()> {
    let clear_blocks = MAXIMUM_RECORD_BLOCKS.min(backing_blocks.saturating_sub(append_block));
    let zero = AlignedBlock([0; BLOCK_SIZE]);
    for block_index in 0..clear_blocks {
        let write = opcode::Write::new(
            types::Fd(backing.as_raw_fd()),
            zero.0.as_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset((append_block + block_index) * BLOCK_SIZE as u64)
        .build();
        submit_exact(
            backing_io,
            write,
            append_block + block_index,
            BLOCK_SIZE as i32,
        )?;
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(&format!(
            "{STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT}-{block_index}"
        ))?;
    }

    let fsync = opcode::Fsync::new(types::Fd(backing.as_raw_fd())).build();
    submit_exact(backing_io, fsync, append_block, 0)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    pause_at_test_failpoint(STALE_TAIL_FSYNC_COMPLETE_FAILPOINT)?;
    Ok(())
}

fn submit_exact_raw(
    ring: &mut Option<IoUring>,
    entry: squeue::Entry,
    user_data: u64,
    expected_result: i32,
) -> io::Result<()> {
    // SAFETY: each caller keeps operation buffers and the file alive until this waits for the CQE.
    let push_result = unsafe {
        ring.as_mut()
            .ok_or_else(|| io::Error::other("io_uring unavailable"))?
            .submission()
            .push(&entry.user_data(user_data))
    };
    if push_result.is_err() {
        drop(ring.take());
        return Err(io::Error::other("io_uring submission queue is full"));
    }
    wait_exact(ring, user_data, expected_result)
}

fn wait_exact(ring: &mut Option<IoUring>, user_data: u64, expected_result: i32) -> io::Result<()> {
    wait_exact_with(ring, user_data, expected_result, |ring| {
        ring.submit_and_wait(1)
    })
}

fn wait_exact_with(
    ring: &mut Option<IoUring>,
    user_data: u64,
    expected_result: i32,
    mut submit_and_wait: impl FnMut(&mut IoUring) -> io::Result<usize>,
) -> io::Result<()> {
    let mut submitted = 0;
    let mut unexpected_completion = false;
    loop {
        // A submitted SQE may retain caller pointers even when this wait is interrupted or empty.
        match submit_and_wait(
            ring.as_mut()
                .ok_or_else(|| io::Error::other("io_uring unavailable"))?,
        ) {
            Ok(count) if count <= 1 - submitted => submitted += count,
            Ok(_) => {
                drop(ring.take());
                return Err(io::Error::other("unexpected io_uring submission count"));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                drop(ring.take());
                return Err(error);
            }
        }

        let mut completions = ring
            .as_mut()
            .ok_or_else(|| io::Error::other("io_uring unavailable"))?
            .completion();
        let Some(completion) = completions.next() else {
            continue;
        };
        if completion.user_data() != user_data {
            unexpected_completion = true;
            continue;
        }
        if unexpected_completion || completion.flags() != 0 {
            return Err(io::Error::other("unexpected io_uring completion"));
        }
        if completion.result() < 0 {
            return Err(io::Error::from_raw_os_error(-completion.result()));
        }
        if completion.result() != expected_result {
            return Err(io::Error::other("short io_uring operation"));
        }
        if completions.next().is_some() {
            return Err(io::Error::other("unexpected extra io_uring completion"));
        }
        return Ok(());
    }
}

/// Opens a version-3 volume with the default replay bound and mapping-cache budget.
pub fn open(backing_path: impl AsRef<Path>) -> io::Result<Volume> {
    open_with_options(backing_path, VolumeOpenOptions::default())
}

/// Opens a version-3 volume with the supplied replay, buffer, and cache configuration.
pub fn open_with_options(
    backing_path: impl AsRef<Path>,
    options: VolumeOpenOptions,
) -> io::Result<Volume> {
    format_v3::volume::open(
        backing_path.as_ref(),
        options,
        options.cache_budget_bytes,
        None,
    )
}

/// Opens a formatted volume and accounts its engine-submitted backing I/O.
///
/// Host-filesystem metadata operations used to open and inspect the backing file
/// are outside this accounting scope.
pub fn open_with_accounting(
    backing_path: impl AsRef<Path>,
    options: VolumeOpenOptions,
    accounting: io_accounting::Writer,
) -> io::Result<Volume> {
    format_v3::volume::open(
        backing_path.as_ref(),
        options,
        options.cache_budget_bytes,
        Some(accounting),
    )
}

/// Clamps buffer allocation relative to the log. Admission reserves append space separately.
fn clamp_write_buffer_blocks(
    write_buffer_bytes: u64,
    backing_blocks: u64,
    log_start: u32,
) -> usize {
    let configured = (write_buffer_bytes / BLOCK_SIZE as u64).max(1);
    let log_capacity = backing_blocks.saturating_sub(u64::from(log_start));
    let device_limit = (log_capacity / WRITE_BUFFER_DEVICE_DIVISOR).max(1);
    let blocks = configured.min(device_limit).max(1);
    usize::try_from(blocks).unwrap_or(1)
}

/// Writes version-3 descriptors and empty index bodies to a pre-sized backing file.
pub fn format(backing_path: impl AsRef<Path>, volume_bytes: u64) -> io::Result<()> {
    format_v3::storage::format(backing_path.as_ref(), volume_bytes, None)
}

/// Formats a volume and accounts its engine-submitted backing I/O.
///
/// Host-filesystem metadata and allocation operations are outside this accounting scope.
pub fn format_with_accounting(
    backing_path: impl AsRef<Path>,
    volume_bytes: u64,
    accounting: io_accounting::Writer,
) -> io::Result<()> {
    format_v3::storage::format(backing_path.as_ref(), volume_bytes, Some(accounting))
}

#[cfg(test)]
mod tests;
