#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

mod accounted_io;
mod format_v4;
mod mapping_cache;
mod wakeup;

pub mod io_accounting;
pub mod logging;

use accounted_io::{BackingIo, submit_exact};
pub use format_v4::budget::{
    DEFAULT_HEAP_PROPORTION_PPM, HeapBudget, HeapBudgetOptions, MINIMUM_HEAP_BYTES,
};
pub use format_v4::log::LogWrites;
pub use format_v4::metrics::{MappingIo, MappingStats, SpaceUsage};
pub use format_v4::reclaim::ReclaimStats;
pub use format_v4::reclaim::scheduler::{DEFAULT_IDLE_WAIT, DEFAULT_MAXIMUM_INTERVAL, Scheduler};
pub use format_v4::reclaim::select::SelectionMode;
pub use format_v4::storage::FormatOptions;
pub use format_v4::volume::Volume;
pub use format_v4::volume::reclaim::{BackingDiscard, MaintenanceStep};
pub use mapping_cache::sizing::Sizing as CacheSizing;
pub use mapping_cache::{Counters as CacheCounters, Lookups, Snapshot as CacheSnapshot};

const DEFAULT_CHECKPOINT_AFTER_BYTES: u64 = 64 * 1024 * 1024;
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
static CONFIGURED_FAILPOINT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The failpoint selected through the environment, read once so that checks allocate nothing.
#[cfg(any(test, feature = "test-failpoints"))]
fn configured_failpoint() -> Option<&'static str> {
    CONFIGURED_FAILPOINT
        .get_or_init(|| std::env::var(TEST_FAILPOINT_ENV).ok())
        .as_deref()
}

#[cfg(any(test, feature = "test-failpoints"))]
fn pause_at_test_failpoint(name: &str) -> io::Result<()> {
    if configured_failpoint() != Some(name) {
        return Ok(());
    }
    pause_at_failpoint(name, None)
}

/// Pauses at `prefix-index` without building the name on the heap.
#[cfg(any(test, feature = "test-failpoints"))]
fn pause_at_indexed_failpoint(prefix: &str, index: u64) -> io::Result<()> {
    let Some(configured) = configured_failpoint() else {
        return Ok(());
    };
    let Some(suffix) = configured.strip_prefix(prefix) else {
        return Ok(());
    };
    let Some(digits) = suffix.strip_prefix('-') else {
        return Ok(());
    };
    if digits.parse::<u64>() != Ok(index) {
        return Ok(());
    }
    pause_at_failpoint(prefix, Some(index))
}

#[cfg(any(test, feature = "test-failpoints"))]
fn pause_at_failpoint(name: &str, index: Option<u64>) -> io::Result<()> {
    match index {
        Some(index) => println!("{name}-{index}"),
        None => println!("{name}"),
    }
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
    /// Space pressure: buffered overwrites keep victims' old payloads alive until released.
    Pressure,
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
    /// Releases triggered by space pressure while victims waited on buffered overwrites.
    pub pressure_releases: u64,
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
            "records={} payloads={} payloads_per_record={:.3} backing_over_payload={:.3} footer_bytes={} payload_bytes={} full_releases={} flush_releases={} pressure_releases={} histogram={:?}",
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
            self.pressure_releases,
            self.payload_histogram,
        )
    }
}

/// Configuration for opening a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeOpenOptions {
    /// Maximum physical log bytes appended on both heads after the selected checkpoint.
    pub checkpoint_after_bytes: u64,
    /// Write-back buffer capacity in bytes, clamped to the device at open. A one-block value keeps
    /// write-through semantics; larger values coalesce writes and release them on flush or FUA.
    pub write_buffer_bytes: u64,
    /// Explicit mapping-cache share of the heap budget; None gives the cache whatever remains
    /// after the write buffer, the cleaner share and supporting state, capped at the volume's
    /// mapping pages. Must hold min(effective buffer slots, volume mapping pages) pages.
    pub cache_budget_bytes: Option<u64>,
    /// The proportional heap budget and its division (ADR-08).
    pub heap: HeapBudgetOptions,
    /// Backing bytes the preallocated segment state can grow to; None allows twice the
    /// provisioned backing.
    pub growth_capacity_bytes: Option<u64>,
    /// How freed segments are returned to the backing.
    pub backing_discard: BackingDiscard,
}

impl Default for VolumeOpenOptions {
    fn default() -> Self {
        Self {
            checkpoint_after_bytes: DEFAULT_CHECKPOINT_AFTER_BYTES,
            write_buffer_bytes: MINIMUM_WRITE_BUFFER_BYTES,
            cache_budget_bytes: None,
            heap: HeapBudgetOptions::default(),
            growth_capacity_bytes: None,
            backing_discard: BackingDiscard::default(),
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
        if self.heap.proportion_ppm == 0 || self.heap.proportion_ppm > 1_000_000 {
            return Err(invalid_input(
                "heap proportion must be within 1..=1_000_000 ppm",
            ));
        }
        if self.heap.total_bytes == Some(0) || self.heap.cleaner_bytes == Some(0) {
            return Err(invalid_input("heap shares must be nonzero"));
        }
        Ok(())
    }
}

/// A preallocated, 4 KiB-aligned write-back buffer indexed by logical block address.
struct WriteBackBuffer {
    arena: Vec<AlignedBlock>,
    lbas: Vec<u32>,
    slot_of_lba: hashbrown::HashMap<u32, usize, std::hash::RandomState>,
    /// Preallocated scratch for counting distinct mapping pages without allocating.
    page_scratch: Vec<u32>,
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
        // Twice the live entries lets the pinned hashbrown rehash tombstones in place.
        let mut slot_of_lba = hashbrown::HashMap::with_hasher(std::hash::RandomState::new());
        slot_of_lba.reserve(capacity_blocks * 2);
        let mut page_scratch = Vec::new();
        page_scratch.reserve_exact(capacity_blocks);
        Self {
            arena,
            lbas,
            slot_of_lba,
            page_scratch,
        }
    }

    /// Bytes the lookup table allocated, for the heap budget.
    fn lookup_bytes(&self) -> usize {
        self.slot_of_lba.allocation_size()
    }

    /// Drops every buffered payload inside `lba..lba + count`. Returns whether any was dropped.
    /// Arena slots stay stable for the remaining payloads: the vacated slot is refilled from the
    /// end of the LBA list, so lookups keep pointing at moved payloads.
    fn remove_range(&mut self, lba: u32, count: u32) -> bool {
        let mut removed = false;
        let mut index = 0;
        // Bounded by the buffer: each iteration removes an entry or advances.
        while index < self.lbas.len() {
            let candidate = self.lbas[index];
            if candidate < lba || candidate - lba >= count {
                index += 1;
                continue;
            }
            let last = self.lbas.len() - 1;
            if index != last {
                let moved = self.lbas[last];
                let (from, to) = (last, index);
                let payload = self.arena[from].0;
                self.arena[to].0.copy_from_slice(&payload);
                self.lbas[to] = moved;
                self.slot_of_lba.insert(moved, to);
            }
            self.lbas.pop();
            self.slot_of_lba.remove(&candidate);
            removed = true;
        }
        removed
    }

    /// Distinct mapping pages touched by the buffered payloads.
    fn distinct_pages(&mut self) -> usize {
        self.page_scratch.clear();
        self.page_scratch.extend(
            self.lbas
                .iter()
                .map(|lba| lba / mapping_cache::ENTRIES_PER_PAGE as u32),
        );
        self.page_scratch.sort_unstable();
        self.page_scratch.dedup();
        self.page_scratch.len()
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
    /// the payload occupies a new slot, i.e. when it reserves fresh log space. Payloads stay in the
    /// arena slot they were stored in; sorting `lbas` never moves them.
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

/// Opens a format-4 volume with the default replay bound, heap budget and cache share.
pub fn open(backing_path: impl AsRef<Path>) -> io::Result<Volume> {
    open_with_options(backing_path, VolumeOpenOptions::default())
}

/// Opens a format-4 volume with the supplied replay, buffer, cache and heap configuration.
pub fn open_with_options(
    backing_path: impl AsRef<Path>,
    options: VolumeOpenOptions,
) -> io::Result<Volume> {
    format_v4::volume::open(backing_path.as_ref(), options, None)
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
    format_v4::volume::open(backing_path.as_ref(), options, Some(accounting))
}

/// Formats a format-4 volume with the default segment size for the backing.
pub fn format(backing_path: impl AsRef<Path>, volume_bytes: u64) -> io::Result<()> {
    format_v4::storage::format(
        backing_path.as_ref(),
        volume_bytes,
        FormatOptions::default(),
        None,
    )
}

/// Formats a format-4 volume with explicit format-time options.
pub fn format_with_options(
    backing_path: impl AsRef<Path>,
    volume_bytes: u64,
    options: FormatOptions,
) -> io::Result<()> {
    format_v4::storage::format(backing_path.as_ref(), volume_bytes, options, None)
}

/// Formats a volume and accounts its engine-submitted backing I/O.
///
/// Host-filesystem metadata and allocation operations are outside this accounting scope.
pub fn format_with_accounting(
    backing_path: impl AsRef<Path>,
    volume_bytes: u64,
    accounting: io_accounting::Writer,
) -> io::Result<()> {
    format_with_options_and_accounting(
        backing_path,
        volume_bytes,
        FormatOptions::default(),
        accounting,
    )
}

/// Formats a volume with explicit format-time options and accounted backing I/O.
pub fn format_with_options_and_accounting(
    backing_path: impl AsRef<Path>,
    volume_bytes: u64,
    options: FormatOptions,
    accounting: io_accounting::Writer,
) -> io::Result<()> {
    format_v4::storage::format(
        backing_path.as_ref(),
        volume_bytes,
        options,
        Some(accounting),
    )
}

#[cfg(test)]
mod tests;
