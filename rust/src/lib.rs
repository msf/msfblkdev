#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

mod checkpoint;

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

#[repr(align(4096))]
struct AlignedRecord([u8; 2 * BLOCK_SIZE]);

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

/// Configuration for opening a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeOpenOptions {
    /// Maximum physical log bytes retained after the selected checkpoint.
    pub checkpoint_after_bytes: u64,
}

impl Default for VolumeOpenOptions {
    fn default() -> Self {
        Self {
            checkpoint_after_bytes: DEFAULT_CHECKPOINT_AFTER_BYTES,
        }
    }
}

pub struct Volume {
    backing: File,
    ring: Option<IoUring>,
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
    failed: bool,
}

impl Volume {
    /// Returns the number of addressable 4 KiB logical blocks.
    pub fn volume_blocks(&self) -> u32 {
        self.volume_blocks
    }

    /// Makes all completed writes durable.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        let fsync = opcode::Fsync::new(types::Fd(self.backing.as_raw_fd())).build();
        if let Err(error) = submit_exact(&mut self.ring, fsync, self.last_lsn, 0) {
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
        if self.last_lsn == self.checkpoint_lsn {
            self.flush()
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

        let physical_block = self.physical_blocks[lba as usize];
        if physical_block == 0 {
            data.fill(0);
            return Ok(());
        }

        let mut payload = AlignedBlock([0; BLOCK_SIZE]);
        let read = opcode::Read::new(
            types::Fd(self.backing.as_raw_fd()),
            payload.0.as_mut_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(u64::from(physical_block) * BLOCK_SIZE as u64)
        .build();
        if let Err(error) = submit_exact(&mut self.ring, read, u64::from(lba), BLOCK_SIZE as i32) {
            self.failed = true;
            return Err(error);
        }
        if payload_checksum(self.volume_id, lba, physical_block, &payload.0)
            != self.checksums[lba as usize]
        {
            return Err(invalid_data("payload checksum mismatch"));
        }
        data.copy_from_slice(&payload.0);
        Ok(())
    }

    /// Appends one payload and footer record and publishes it after exact completion.
    pub fn write_block(&mut self, lba: u32, data: &[u8; BLOCK_SIZE]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        if lba >= self.volume_blocks {
            return Err(invalid_input("invalid logical block"));
        }
        let lsn = self
            .last_lsn
            .checked_add(1)
            .ok_or_else(|| io::Error::other("sequence exhausted"))?;
        let payload_block = u64::from(self.last_footer_block) + 1;
        let footer_block = payload_block + 1;
        if footer_block >= self.backing_blocks {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let record_bytes = (2 * BLOCK_SIZE) as u64;
        let bytes_after_write = self
            .log_bytes_since_checkpoint
            .checked_add(record_bytes)
            .ok_or_else(|| io::Error::other("checkpoint byte count overflow"))?;
        if bytes_after_write > self.checkpoint_after_bytes {
            self.publish_checkpoint()?;
        }

        let payload_block = payload_block as u32;
        let footer_block = footer_block as u32;
        let checksum = payload_checksum(self.volume_id, lba, payload_block, data);

        let mut record = AlignedRecord([0; 2 * BLOCK_SIZE]);
        record.0[..BLOCK_SIZE].copy_from_slice(data);
        encode_write_footer(
            &mut record.0[BLOCK_SIZE..],
            self.volume_id,
            lsn,
            self.last_footer_block,
            footer_block,
            lba,
            checksum,
        );
        let iovecs = [
            libc::iovec {
                iov_base: record.0.as_mut_ptr().cast(),
                iov_len: BLOCK_SIZE,
            },
            libc::iovec {
                iov_base: record.0[BLOCK_SIZE..].as_mut_ptr().cast(),
                iov_len: BLOCK_SIZE,
            },
        ];
        let write = opcode::Writev::new(
            types::Fd(self.backing.as_raw_fd()),
            iovecs.as_ptr(),
            iovecs.len() as u32,
        )
        .offset(u64::from(payload_block) * BLOCK_SIZE as u64)
        .build();
        if let Err(error) = submit_exact(&mut self.ring, write, lsn, record.0.len() as i32) {
            self.failed = true;
            return Err(error);
        }
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(RECORD_WRITE_COMPLETE_FAILPOINT)?;

        self.physical_blocks[lba as usize] = payload_block;
        self.checksums[lba as usize] = checksum;
        self.last_lsn = lsn;
        self.last_footer_block = footer_block;
        self.log_bytes_since_checkpoint += record_bytes;
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

fn encode_write_footer(
    footer: &mut [u8],
    volume_id: u64,
    lsn: u64,
    previous_footer_block: u32,
    footer_block: u32,
    lba: u32,
    payload_checksum: u64,
) {
    footer.fill(0);
    footer[..4].copy_from_slice(FOOTER_MAGIC);
    footer[4] = FORMAT_VERSION;
    footer[5] = RECORD_KIND_WRITE;
    footer[8..16].copy_from_slice(&volume_id.to_le_bytes());
    footer[16..24].copy_from_slice(&lsn.to_le_bytes());
    footer[24..28].copy_from_slice(&previous_footer_block.to_le_bytes());
    footer[28..32].copy_from_slice(&footer_block.to_le_bytes());
    footer[FOOTER_LBA_OFFSET..FOOTER_LBA_OFFSET + 4].copy_from_slice(&lba.to_le_bytes());
    footer[FOOTER_PAYLOAD_CHECKSUM_OFFSET..FOOTER_PAYLOAD_CHECKSUM_OFFSET + 8]
        .copy_from_slice(&payload_checksum.to_le_bytes());
    let checksum = xxh3_64(footer);
    footer[FOOTER_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
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
    ring: &mut Option<IoUring>,
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
    submit_exact(ring, read, u64::from(physical_block), BLOCK_SIZE as i32)
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
    ring: &mut Option<IoUring>,
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
        submit_exact(ring, write, append_block + block_index, BLOCK_SIZE as i32)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(&format!(
            "{STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT}-{block_index}"
        ))?;
    }

    let fsync = opcode::Fsync::new(types::Fd(backing.as_raw_fd())).build();
    submit_exact(ring, fsync, append_block, 0)?;
    #[cfg(any(test, feature = "test-failpoints"))]
    pause_at_test_failpoint(STALE_TAIL_FSYNC_COMPLETE_FAILPOINT)?;
    Ok(())
}

fn read_one_tail_record(
    ring: &mut Option<IoUring>,
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
        read_block_exact(ring, backing, footer_block as u32, &mut footer)?;
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

fn submit_exact(
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
    if !options
        .checkpoint_after_bytes
        .is_multiple_of(BLOCK_SIZE as u64)
        || options.checkpoint_after_bytes < MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64
    {
        return Err(invalid_input("invalid checkpoint_after_bytes"));
    }

    let mut backing = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(backing_path)?;
    let backing_bytes = backing.seek(SeekFrom::End(0))?;
    let mut ring = Some(IoUring::new(2)?);
    let mut descriptors = AlignedDescriptors([0; 2 * BLOCK_SIZE]);
    let read = opcode::Read::new(
        types::Fd(backing.as_raw_fd()),
        descriptors.0.as_mut_ptr(),
        descriptors.0.len() as u32,
    )
    .offset(0)
    .build();
    submit_exact(&mut ring, read, 3, descriptors.0.len() as i32)?;

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
            load_checkpoint_body(&mut ring, &backing, checkpoint)?
        {
            recovered = Some((checkpoint, physical_blocks, checksums));
            break;
        }
    }
    let (checkpoint, mut physical_blocks, mut checksums) =
        recovered.ok_or_else(|| invalid_data("no valid checkpoint body"))?;
    let mut replay_cursor = checkpoint;
    let mut log_bytes_since_checkpoint = 0_u64;
    while let Some(record) = read_one_tail_record(&mut ring, &backing, replay_cursor)? {
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
        &mut ring,
        &backing,
        u64::from(last_footer_block) + 1,
        checkpoint.backing_blocks,
    )?;

    Ok(Volume {
        backing,
        ring,
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
        failed: false,
    })
}

/// Writes the two checksummed empty checkpoint roots to a pre-sized backing file.
pub fn format(backing_path: impl AsRef<Path>, volume_bytes: u64) -> io::Result<()> {
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

    let mut ring = Some(IoUring::new(2)?);
    let write = opcode::Write::new(
        types::Fd(file.as_raw_fd()),
        descriptors.0.as_ptr(),
        descriptors.0.len() as u32,
    )
    .offset(0)
    .build();
    submit_exact(&mut ring, write, 1, descriptors.0.len() as i32)?;
    let fsync = opcode::Fsync::new(types::Fd(file.as_raw_fd())).build();
    submit_exact(&mut ring, fsync, 2, 0)
}

#[cfg(test)]
mod tests;
