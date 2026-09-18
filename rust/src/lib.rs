#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

mod accounted_io;
mod checkpoint;
#[cfg(test)]
mod format_v3;
#[cfg(test)]
mod mapping_cache;

pub mod io_accounting;
pub mod logging;

use accounted_io::{BackingIo, submit_exact};
use checkpoint::{
    AlignedDescriptors, Checkpoint, CheckpointSlot, DEFAULT_CHECKPOINT_AFTER_BYTES,
    decode_checkpoint, encode_checkpoint_descriptor, load_checkpoint_body,
};
#[cfg(test)]
use checkpoint::{
    CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT, CHECKPOINT_BODY_CHECKSUM_OFFSET,
    CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT, CHECKPOINT_MAGIC, DESCRIPTOR_CHECKSUM_OFFSET,
    DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT, DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
};

const BLOCK_SIZE: usize = 4096;
const FOOTER_MAGIC: &[u8; 4] = b"VBLF";
const FORMAT_VERSION: u8 = 1;
const RECORD_KIND_WRITE: u8 = 1;
const FOOTER_LBA_OFFSET: usize = 32;
const FOOTER_PAYLOAD_CHECKSUM_OFFSET: usize = 1384;
const FOOTER_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
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
// Reserving log at insert time means a full buffer holds ~2x its blocks of reserved log. Keep the
// effective buffer to at most a quarter of the log so one buffer window reserves at most half the
// log; a small device (ublk/fio scenario 7) collapses toward a one-block write-through window.
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

// A checkpoint body streams through a 128 KiB aligned buffer instead of one 4 KiB block at a time.
const CHECKPOINT_BODY_BLOCKS: usize = 32;
#[repr(align(4096))]
struct AlignedBody([u8; CHECKPOINT_BODY_BLOCKS * BLOCK_SIZE]);

/// Preallocated `writev` iovecs reused per release. It only holds pointers into this `Volume`'s own
/// stable heap arena and footer scratch, and is cleared between releases.
struct ReleaseIovecs(Vec<libc::iovec>);

// SAFETY: the iovecs reference arena and footer memory owned by the same `Volume`. That memory is
// heap-allocated and stable for the volume's life, so it stays valid when the `Volume` moves
// between threads, and the vector is empty except during a single-threaded release call.
unsafe impl Send for ReleaseIovecs {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    physical_map_blocks: u32,
    checksum_map_blocks: u32,
    green_physical_map_start: u32,
    green_checksum_map_start: u32,
    blue_physical_map_start: u32,
    blue_checksum_map_start: u32,
    log_start: u32,
}

struct RecoveredWrite {
    lba: u32,
    checksum: u64,
    payload_block: u32,
}

struct RecoveredRecord {
    writes: Vec<RecoveredWrite>,
    lsn: u64,
    footer_block: u32,
}

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
    /// Header, both mapping tables and both checkpoint slots.
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
}

impl Default for VolumeOpenOptions {
    fn default() -> Self {
        Self {
            checkpoint_after_bytes: DEFAULT_CHECKPOINT_AFTER_BYTES,
            write_buffer_bytes: MINIMUM_WRITE_BUFFER_BYTES,
        }
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
}

pub struct Volume {
    backing: File,
    backing_io: BackingIo,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    physical_blocks: Vec<u32>,
    checksums: Vec<u64>,
    last_lsn: u64,
    durable_lsn: u64,
    checkpoint_lsn: u64,
    last_footer_block: u32,
    next_checkpoint_slot: CheckpointSlot,
    log_bytes_since_checkpoint: u64,
    checkpoint_after_bytes: u64,
    write_buffer: WriteBackBuffer,
    footer_scratch: AlignedBlock,
    read_scratch: AlignedBlock,
    release_checksums: Vec<u64>,
    release_iovecs: ReleaseIovecs,
    release_stats: ReleaseStats,
    failed: bool,
}

impl Volume {
    /// Returns the number of addressable 4 KiB logical blocks.
    pub fn volume_blocks(&self) -> u32 {
        self.volume_blocks
    }

    /// Returns a snapshot of the opt-in batching counters.
    #[must_use]
    pub fn release_stats(&self) -> ReleaseStats {
        self.release_stats
    }

    /// Reports physical and logical space use. Reads the backing file's real
    /// allocation from the host filesystem, so it performs one `fstat`.
    pub fn space_usage(&self) -> io::Result<SpaceUsage> {
        let volume_bytes = u64::from(self.volume_blocks) * BLOCK_SIZE as u64;
        let (_, layout) = layout_for(volume_bytes)?;
        let metadata_blocks = u64::from(layout.log_start);
        let live_blocks = self
            .physical_blocks
            .iter()
            .filter(|physical| **physical != 0)
            .count() as u64;
        // The first log block follows the metadata region; an empty log leaves
        // `last_footer_block` one block below it.
        let log_used_blocks = u64::from(self.last_footer_block)
            .saturating_add(1)
            .saturating_sub(metadata_blocks);
        let metadata = self.backing.metadata()?;
        Ok(SpaceUsage {
            volume_bytes,
            live_bytes: live_blocks * BLOCK_SIZE as u64,
            backing_bytes: metadata.len(),
            allocated_bytes: metadata.blocks() * 512,
            metadata_bytes: metadata_blocks * BLOCK_SIZE as u64,
            log_capacity_bytes: self.backing_blocks.saturating_sub(metadata_blocks)
                * BLOCK_SIZE as u64,
            log_used_bytes: log_used_blocks * BLOCK_SIZE as u64,
        })
    }

    /// Releases the write-back buffer and makes all completed writes durable.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        self.release_records(ReleaseTrigger::Flush)?;
        self.fsync_log()
    }

    /// Issues one backing fsync and advances the durable sequence bound.
    fn fsync_log(&mut self) -> io::Result<()> {
        let fsync = opcode::Fsync::new(types::Fd(self.backing.as_raw_fd())).build();
        if let Err(error) = submit_exact(&mut self.backing_io, fsync, self.last_lsn, 0) {
            self.failed = true;
            return Err(error);
        }
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(LOG_FSYNC_COMPLETE_FAILPOINT)?;
        self.durable_lsn = self.last_lsn;
        Ok(())
    }

    /// Flushes and checkpoints changed mapping state before releasing its resources.
    pub fn close(mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        self.release_records(ReleaseTrigger::Flush)?;
        if self.last_lsn == self.checkpoint_lsn {
            self.fsync_log()
        } else {
            self.publish_checkpoint()
        }
    }

    /// Reads one logical block, returning zeros when it has not been written.
    pub fn read_block(&mut self, lba: u32, data: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        if lba >= self.volume_blocks {
            return Err(invalid_input("invalid logical block"));
        }

        if let Some(buffered) = self.write_buffer.get(lba) {
            data.copy_from_slice(buffered);
            return Ok(());
        }

        let physical_block = self.physical_blocks[lba as usize];
        if physical_block == 0 {
            data.fill(0);
            return Ok(());
        }

        // The read overwrites the whole aligned scratch block, so it is never zero-initialized.
        let read = opcode::Read::new(
            types::Fd(self.backing.as_raw_fd()),
            self.read_scratch.0.as_mut_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(u64::from(physical_block) * BLOCK_SIZE as u64)
        .build();
        if let Err(error) = submit_exact(
            &mut self.backing_io,
            read,
            u64::from(lba),
            BLOCK_SIZE as i32,
        ) {
            self.failed = true;
            return Err(error);
        }
        if payload_checksum(self.volume_id, lba, physical_block, &self.read_scratch.0)
            != self.checksums[lba as usize]
        {
            return Err(invalid_data("payload checksum mismatch"));
        }
        data.copy_from_slice(&self.read_scratch.0);
        Ok(())
    }

    /// Buffers one payload, reserving log space, and completes immediately. Repeated writes to one
    /// address overwrite in place; a full buffer or a 338-payload window releases before returning.
    pub fn write_block(&mut self, lba: u32, data: &[u8; BLOCK_SIZE]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        if lba >= self.volume_blocks {
            return Err(invalid_input("invalid logical block"));
        }
        if self.write_buffer.get(lba).is_some() {
            self.write_buffer.store(lba, data);
            return Ok(());
        }
        // Admit the write only if the log can still hold every buffered payload, including this
        // one, at its worst case. The check precedes the store, so the offending write returns
        // ENOSPC rather than a later flush. Released payloads are already in last_footer_block,
        // so nothing is reserved twice and batching is not charged write-through economics.
        let pending = pending_log_blocks(self.write_buffer.len() + 1);
        let last_needed = u64::from(self.last_footer_block)
            .checked_add(pending)
            .ok_or_else(|| io::Error::other("log accounting overflow"))?;
        if last_needed >= self.backing_blocks {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let is_new = self.write_buffer.store(lba, data);
        debug_assert!(is_new, "a miss in get must store a new slot");
        if self.write_buffer.len() == self.write_buffer.capacity() {
            self.release_records(ReleaseTrigger::Full)?;
        }
        Ok(())
    }

    /// Writes every buffered payload as one or more records (each `writev`), advancing the log and
    /// mapping. It does not fsync: durability is the caller's flush or a checkpoint.
    fn release_records(&mut self, trigger: ReleaseTrigger) -> io::Result<()> {
        let total = self.write_buffer.len();
        if total > 0 {
            match trigger {
                ReleaseTrigger::Full => self.release_stats.full_releases += 1,
                ReleaseTrigger::Flush => self.release_stats.flush_releases += 1,
            }
        }
        let mut start = 0;
        while start < total {
            let count = (total - start).min(MAXIMUM_PAYLOAD_BLOCKS);
            let record_bytes = (count as u64 + 1) * BLOCK_SIZE as u64;
            let bytes_after = self
                .log_bytes_since_checkpoint
                .checked_add(record_bytes)
                .ok_or_else(|| io::Error::other("checkpoint byte count overflow"))?;
            if bytes_after > self.checkpoint_after_bytes {
                self.publish_checkpoint()?;
            }
            self.write_one_record(start, count)?;
            start += count;
        }
        self.write_buffer.clear();
        Ok(())
    }

    /// Emits one record covering `count` buffered payloads starting at buffer slot `start`.
    fn write_one_record(&mut self, start: usize, count: usize) -> io::Result<()> {
        assert!(
            (1..=MAXIMUM_PAYLOAD_BLOCKS).contains(&count),
            "record payload count"
        );
        let lsn = self
            .last_lsn
            .checked_add(1)
            .ok_or_else(|| io::Error::other("sequence exhausted"))?;
        let first_payload_block = u64::from(self.last_footer_block) + 1;
        let footer_block = first_payload_block + count as u64;
        // Reservation guarantees room, but assert the invariant that keeps the write in bounds.
        assert!(
            footer_block < self.backing_blocks,
            "release exceeds reserved log"
        );
        let footer_block = footer_block as u32;

        self.release_checksums.clear();
        for index in 0..count {
            let lba = self.write_buffer.lbas[start + index];
            let payload_block = first_payload_block as u32 + index as u32;
            let payload = &self.write_buffer.arena[start + index].0;
            self.release_checksums.push(payload_checksum(
                self.volume_id,
                lba,
                payload_block,
                payload,
            ));
        }
        encode_write_footer_multi(
            &mut self.footer_scratch.0,
            self.volume_id,
            lsn,
            self.last_footer_block,
            footer_block,
            &self.write_buffer.lbas[start..start + count],
            &self.release_checksums,
        );

        // Reuse the preallocated iovec scratch: no heap allocation on the release path.
        let iovecs = &mut self.release_iovecs.0;
        iovecs.clear();
        for index in 0..count {
            iovecs.push(libc::iovec {
                iov_base: self.write_buffer.arena[start + index].0.as_ptr() as *mut libc::c_void,
                iov_len: BLOCK_SIZE,
            });
        }
        iovecs.push(libc::iovec {
            iov_base: self.footer_scratch.0.as_ptr() as *mut libc::c_void,
            iov_len: BLOCK_SIZE,
        });

        let record_bytes = (count + 1) * BLOCK_SIZE;
        let write = opcode::Writev::new(
            types::Fd(self.backing.as_raw_fd()),
            iovecs.as_ptr(),
            iovecs.len() as u32,
        )
        .offset(first_payload_block * BLOCK_SIZE as u64)
        .build();
        if let Err(error) = submit_exact(&mut self.backing_io, write, lsn, record_bytes as i32) {
            self.failed = true;
            return Err(error);
        }
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(RECORD_WRITE_COMPLETE_FAILPOINT)?;

        for index in 0..count {
            let lba = self.write_buffer.lbas[start + index] as usize;
            self.physical_blocks[lba] = first_payload_block as u32 + index as u32;
            self.checksums[lba] = self.release_checksums[index];
        }
        self.last_lsn = lsn;
        self.last_footer_block = footer_block;
        self.log_bytes_since_checkpoint += record_bytes as u64;
        self.release_stats.record(count);
        log_debug!(
            "event=release_record lsn={lsn} payloads={count} footer_block={footer_block} record_bytes={record_bytes} buffered={}",
            self.write_buffer.len()
        );
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(MAPPING_PUBLISHED_FAILPOINT)?;
        Ok(())
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

fn encode_write_footer_multi(
    footer: &mut [u8],
    volume_id: u64,
    lsn: u64,
    previous_footer_block: u32,
    footer_block: u32,
    lbas: &[u32],
    checksums: &[u64],
) {
    assert_eq!(lbas.len(), checksums.len(), "footer entries must be paired");
    assert!(
        !lbas.is_empty() && lbas.len() <= MAXIMUM_PAYLOAD_BLOCKS,
        "footer payload count out of range"
    );
    footer.fill(0);
    footer[..4].copy_from_slice(FOOTER_MAGIC);
    footer[4] = FORMAT_VERSION;
    footer[5] = RECORD_KIND_WRITE;
    footer[8..16].copy_from_slice(&volume_id.to_le_bytes());
    footer[16..24].copy_from_slice(&lsn.to_le_bytes());
    footer[24..28].copy_from_slice(&previous_footer_block.to_le_bytes());
    footer[28..32].copy_from_slice(&footer_block.to_le_bytes());
    for (index, (lba, checksum)) in lbas.iter().zip(checksums).enumerate() {
        let lba_offset = FOOTER_LBA_OFFSET + index * size_of::<u32>();
        let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + index * size_of::<u64>();
        footer[lba_offset..lba_offset + size_of::<u32>()].copy_from_slice(&lba.to_le_bytes());
        footer[checksum_offset..checksum_offset + size_of::<u64>()]
            .copy_from_slice(&checksum.to_le_bytes());
    }
    let checksum = xxh3_64(footer);
    footer[FOOTER_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
}

/// Log blocks a release of `payloads` buffered blocks consumes at worst.
///
/// A release drains the whole buffer into records of at most `MAXIMUM_PAYLOAD_BLOCKS`
/// payloads, each followed by one footer, so the cost is the payloads plus one
/// footer per record. This is exact, not pessimistic: it equals 2 for a single
/// payload and 339 for a full record.
const fn pending_log_blocks(payloads: usize) -> u64 {
    let payloads = payloads as u64;
    payloads + payloads.div_ceil(MAXIMUM_PAYLOAD_BLOCKS as u64)
}

fn layout_for(volume_bytes: u64) -> io::Result<(u32, Layout)> {
    if volume_bytes == 0 || !volume_bytes.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(invalid_input("invalid volume size"));
    }

    let volume_blocks = volume_bytes / BLOCK_SIZE as u64;
    if volume_blocks > MAX_VOLUME_BLOCKS {
        return Err(invalid_input("invalid volume size"));
    }

    let physical_map_blocks = volume_blocks.div_ceil(BLOCK_SIZE as u64 / size_of::<u32>() as u64);
    let checksum_map_blocks = volume_blocks.div_ceil(BLOCK_SIZE as u64 / size_of::<u64>() as u64);
    let physical_map_blocks = physical_map_blocks as u32;
    let checksum_map_blocks = checksum_map_blocks as u32;
    let green_physical_map_start = 2;
    let green_checksum_map_start = green_physical_map_start + physical_map_blocks;
    let blue_physical_map_start = green_checksum_map_start + checksum_map_blocks;
    let blue_checksum_map_start = blue_physical_map_start + physical_map_blocks;

    Ok((
        volume_blocks as u32,
        Layout {
            physical_map_blocks,
            checksum_map_blocks,
            green_physical_map_start,
            green_checksum_map_start,
            blue_physical_map_start,
            blue_checksum_map_start,
            log_start: blue_checksum_map_start + checksum_map_blocks,
        },
    ))
}

fn backing_blocks_for(layout: &Layout, backing_bytes: u64) -> io::Result<u64> {
    if !backing_bytes.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(invalid_input("backing size is not block aligned"));
    }

    let backing_blocks = backing_bytes / BLOCK_SIZE as u64;
    if backing_blocks > MAX_BACKING_BLOCKS {
        return Err(invalid_input("backing exceeds physical address space"));
    }
    if backing_blocks < u64::from(layout.log_start) + 2 {
        return Err(invalid_input(
            "backing has no room for the initial log record",
        ));
    }
    Ok(backing_blocks)
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

fn decode_write_tail_footer(
    footer: &mut [u8],
    checkpoint: Checkpoint,
    footer_block: u32,
) -> Option<Vec<RecoveredWrite>> {
    let payload_count = footer_block
        .checked_sub(checkpoint.last_footer_block)?
        .checked_sub(1)?;
    if payload_count == 0 || payload_count as usize > MAXIMUM_PAYLOAD_BLOCKS {
        return None;
    }

    let stored_checksum = read_u64(footer, FOOTER_CHECKSUM_OFFSET);
    footer[FOOTER_CHECKSUM_OFFSET..].fill(0);
    let checksum_valid = xxh3_64(footer) == stored_checksum;
    footer[FOOTER_CHECKSUM_OFFSET..].copy_from_slice(&stored_checksum.to_le_bytes());
    if !checksum_valid
        || &footer[..4] != FOOTER_MAGIC
        || footer[4] != FORMAT_VERSION
        || footer[5] != RECORD_KIND_WRITE
        || footer[6..8] != [0, 0]
        || read_u64(footer, 8) != checkpoint.volume_id
        || read_u64(footer, 16) != checkpoint.checkpoint_lsn.checked_add(1)?
        || read_u32(footer, 24) != checkpoint.last_footer_block
        || read_u32(footer, 28) != footer_block
    {
        return None;
    }

    let payload_count = payload_count as usize;
    let used_lba_end = FOOTER_LBA_OFFSET + payload_count * size_of::<u32>();
    let used_checksum_end = FOOTER_PAYLOAD_CHECKSUM_OFFSET + payload_count * size_of::<u64>();
    if footer[used_lba_end..FOOTER_PAYLOAD_CHECKSUM_OFFSET]
        .iter()
        .any(|byte| *byte != 0)
        || footer[used_checksum_end..FOOTER_CHECKSUM_OFFSET]
            .iter()
            .any(|byte| *byte != 0)
    {
        return None;
    }

    let mut entries = Vec::with_capacity(payload_count);
    for index in 0..payload_count {
        let lba_offset = FOOTER_LBA_OFFSET + index * size_of::<u32>();
        let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + index * size_of::<u64>();
        let lba = read_u32(footer, lba_offset);
        if lba >= checkpoint.volume_blocks
            || entries
                .iter()
                .any(|entry: &RecoveredWrite| entry.lba == lba)
        {
            return None;
        }
        entries.push(RecoveredWrite {
            lba,
            checksum: read_u64(footer, checksum_offset),
            payload_block: checkpoint.last_footer_block + 1 + index as u32,
        });
    }
    Some(entries)
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

fn read_one_tail_record(
    backing_io: &mut BackingIo,
    backing: &File,
    checkpoint: Checkpoint,
) -> io::Result<Option<RecoveredRecord>> {
    let first_footer_block = u64::from(checkpoint.last_footer_block) + 2;
    let footer_limit = (u64::from(checkpoint.last_footer_block) + MAXIMUM_RECORD_BLOCKS)
        .min(checkpoint.backing_blocks.saturating_sub(1));
    if first_footer_block > footer_limit {
        return Ok(None);
    }

    let mut footer = AlignedBlock([0; BLOCK_SIZE]);
    for footer_block in first_footer_block..=footer_limit {
        read_block_exact(backing_io, backing, footer_block as u32, &mut footer)?;
        if let Some(entries) =
            decode_write_tail_footer(&mut footer.0, checkpoint, footer_block as u32)
        {
            return Ok(Some(RecoveredRecord {
                writes: entries,
                lsn: checkpoint.checkpoint_lsn + 1,
                footer_block: footer_block as u32,
            }));
        }
    }
    Ok(None)
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

/// Opens a formatted volume using the default 64 MiB replay bound.
pub fn open(backing_path: impl AsRef<Path>) -> io::Result<Volume> {
    open_with_options(backing_path, VolumeOpenOptions::default())
}

/// Opens a formatted volume with the supplied replay-bound configuration.
pub fn open_with_options(
    backing_path: impl AsRef<Path>,
    options: VolumeOpenOptions,
) -> io::Result<Volume> {
    open_internal(backing_path.as_ref(), options, None)
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
    open_internal(backing_path.as_ref(), options, Some(accounting))
}

fn open_internal(
    backing_path: &Path,
    options: VolumeOpenOptions,
    accounting: Option<io_accounting::Writer>,
) -> io::Result<Volume> {
    if !options
        .checkpoint_after_bytes
        .is_multiple_of(BLOCK_SIZE as u64)
        || options.checkpoint_after_bytes < MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64
    {
        return Err(invalid_input("invalid checkpoint_after_bytes"));
    }
    if !options.write_buffer_bytes.is_multiple_of(BLOCK_SIZE as u64)
        || options.write_buffer_bytes < MINIMUM_WRITE_BUFFER_BYTES
        || options.write_buffer_bytes > MAXIMUM_WRITE_BUFFER_BYTES
    {
        return Err(invalid_input("invalid write_buffer_bytes"));
    }

    let mut backing = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(backing_path)?;
    let backing_bytes = backing.seek(SeekFrom::End(0))?;
    let mut backing_io = BackingIo::new(accounting)?;
    let mut descriptors = AlignedDescriptors([0; 2 * BLOCK_SIZE]);
    let read = opcode::Read::new(
        types::Fd(backing.as_raw_fd()),
        descriptors.0.as_mut_ptr(),
        descriptors.0.len() as u32,
    )
    .offset(0)
    .build();
    submit_exact(&mut backing_io, read, 3, descriptors.0.len() as i32)?;

    let (green_bytes, blue_bytes) = descriptors.0.split_at_mut(BLOCK_SIZE);
    let green = decode_checkpoint(green_bytes, CheckpointSlot::Green, backing_bytes);
    let blue = decode_checkpoint(blue_bytes, CheckpointSlot::Blue, backing_bytes);
    if let (Some(green), Some(blue)) = (green, blue) {
        if green.volume_id != blue.volume_id
            || green.volume_blocks != blue.volume_blocks
            || green.backing_blocks != blue.backing_blocks
            || green.layout != blue.layout
        {
            return Err(invalid_data("checkpoint roots disagree"));
        }
        if green.checkpoint_lsn == blue.checkpoint_lsn
            && (green.last_footer_block != blue.last_footer_block
                || green.body_checksum != blue.body_checksum)
        {
            return Err(invalid_data("equal-LSN checkpoint roots disagree"));
        }
    }
    let mut candidates: Vec<_> = [green, blue].into_iter().flatten().collect();
    candidates.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.checkpoint_lsn));
    let mut recovered = None;
    for checkpoint in candidates {
        if let Some((physical_blocks, checksums)) =
            load_checkpoint_body(&mut backing_io, &backing, checkpoint)?
        {
            recovered = Some((checkpoint, physical_blocks, checksums));
            break;
        }
    }
    let (checkpoint, mut physical_blocks, mut checksums) =
        recovered.ok_or_else(|| invalid_data("no valid checkpoint body"))?;
    let mut replay_cursor = checkpoint;
    let mut log_bytes_since_checkpoint = 0_u64;
    while let Some(record) = read_one_tail_record(&mut backing_io, &backing, replay_cursor)? {
        for write in record.writes {
            physical_blocks[write.lba as usize] = write.payload_block;
            checksums[write.lba as usize] = write.checksum;
        }
        let record_blocks = record.footer_block - replay_cursor.last_footer_block;
        replay_cursor.checkpoint_lsn = record.lsn;
        replay_cursor.last_footer_block = record.footer_block;
        log_bytes_since_checkpoint = log_bytes_since_checkpoint
            .checked_add(u64::from(record_blocks) * BLOCK_SIZE as u64)
            .ok_or_else(|| invalid_data("replayed log byte count overflow"))?;
    }
    let last_lsn = replay_cursor.checkpoint_lsn;
    let last_footer_block = replay_cursor.last_footer_block;
    clear_stale_tail(
        &mut backing_io,
        &backing,
        u64::from(last_footer_block) + 1,
        checkpoint.backing_blocks,
    )?;

    let write_buffer_blocks = clamp_write_buffer_blocks(
        options.write_buffer_bytes,
        checkpoint.backing_blocks,
        checkpoint.layout.log_start,
    );
    let mut release_checksums = Vec::new();
    release_checksums.reserve_exact(MAXIMUM_PAYLOAD_BLOCKS);
    let mut release_iovecs = Vec::new();
    release_iovecs.reserve_exact(MAXIMUM_RECORD_BLOCKS as usize);

    Ok(Volume {
        backing,
        backing_io,
        volume_id: checkpoint.volume_id,
        volume_blocks: checkpoint.volume_blocks,
        backing_blocks: checkpoint.backing_blocks,
        physical_blocks,
        checksums,
        last_lsn,
        durable_lsn: last_lsn,
        checkpoint_lsn: checkpoint.checkpoint_lsn,
        last_footer_block,
        next_checkpoint_slot: match checkpoint.slot {
            CheckpointSlot::Green => CheckpointSlot::Blue,
            CheckpointSlot::Blue => CheckpointSlot::Green,
        },
        log_bytes_since_checkpoint,
        checkpoint_after_bytes: options.checkpoint_after_bytes,
        write_buffer: WriteBackBuffer::with_capacity(write_buffer_blocks),

        footer_scratch: AlignedBlock([0; BLOCK_SIZE]),
        read_scratch: AlignedBlock([0; BLOCK_SIZE]),
        release_checksums,
        release_iovecs: ReleaseIovecs(release_iovecs),
        release_stats: ReleaseStats::default(),
        failed: false,
    })
}

/// Clamps the configured write-back buffer to a fraction of the device log so one buffer window
/// cannot exhaust the log; a small device collapses to a one-block write-through window.
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

/// Writes the two checksummed empty checkpoint roots to a pre-sized backing file.
pub fn format(backing_path: impl AsRef<Path>, volume_bytes: u64) -> io::Result<()> {
    format_internal(backing_path.as_ref(), volume_bytes, None)
}

/// Formats a volume and accounts its engine-submitted backing I/O.
///
/// Host-filesystem metadata and allocation operations are outside this accounting scope.
pub fn format_with_accounting(
    backing_path: impl AsRef<Path>,
    volume_bytes: u64,
    accounting: io_accounting::Writer,
) -> io::Result<()> {
    format_internal(backing_path.as_ref(), volume_bytes, Some(accounting))
}

fn format_internal(
    backing_path: &Path,
    volume_bytes: u64,
    accounting: Option<io_accounting::Writer>,
) -> io::Result<()> {
    let (volume_blocks, layout) = layout_for(volume_bytes)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(backing_path)?;
    let backing_blocks = backing_blocks_for(&layout, file.seek(SeekFrom::End(0))?)?;

    let volume_id = random_volume_id()?;

    let mut descriptors = AlignedDescriptors([0; 2 * BLOCK_SIZE]);
    let mut checkpoint = Checkpoint {
        slot: CheckpointSlot::Green,
        volume_id,
        volume_blocks,
        backing_blocks,
        checkpoint_lsn: 0,
        last_footer_block: layout.log_start - 1,
        layout,
        body_checksum: 0,
    };
    encode_checkpoint_descriptor(&mut descriptors.0[..BLOCK_SIZE], &checkpoint);
    checkpoint.slot = CheckpointSlot::Blue;
    encode_checkpoint_descriptor(&mut descriptors.0[BLOCK_SIZE..], &checkpoint);

    let mut backing_io = BackingIo::new(accounting)?;
    let write = opcode::Write::new(
        types::Fd(file.as_raw_fd()),
        descriptors.0.as_ptr(),
        descriptors.0.len() as u32,
    )
    .offset(0)
    .build();
    submit_exact(&mut backing_io, write, 1, descriptors.0.len() as i32)?;
    let fsync = opcode::Fsync::new(types::Fd(file.as_raw_fd())).build();
    submit_exact(&mut backing_io, fsync, 2, 0)
}

#[cfg(test)]
mod tests;
