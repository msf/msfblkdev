#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

const BLOCK_SIZE: usize = 4096;
const CHECKPOINT_MAGIC: &[u8; 4] = b"VBLC";
const FOOTER_MAGIC: &[u8; 4] = b"VBLF";
const FORMAT_VERSION: u8 = 1;
const RECORD_KIND_WRITE: u8 = 1;
const FOOTER_LBA_OFFSET: usize = 32;
const FOOTER_PAYLOAD_CHECKSUM_OFFSET: usize = 1384;
const FOOTER_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
const CHECKPOINT_BODY_CHECKSUM_OFFSET: usize = 44;
const DESCRIPTOR_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
const MAX_VOLUME_BLOCKS: u64 = 1 << 31;
const MAX_BACKING_BLOCKS: u64 = u32::MAX as u64 + 1;
const MAXIMUM_RECORD_BLOCKS: u64 = 339;
const MAXIMUM_PAYLOAD_BLOCKS: usize = MAXIMUM_RECORD_BLOCKS as usize - 1;
const DEFAULT_CHECKPOINT_AFTER_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(test)]
const TEST_FAILPOINT_ENV: &str = "BLOCK_STORAGE_TEST_FAILPOINT";
#[cfg(test)]
const RECORD_WRITE_COMPLETE_FAILPOINT: &str = "record-write-complete";
#[cfg(test)]
const MAPPING_PUBLISHED_FAILPOINT: &str = "mapping-published";
#[cfg(test)]
const LOG_FSYNC_COMPLETE_FAILPOINT: &str = "log-fsync-complete";
#[cfg(test)]
const CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT: &str = "checkpoint-body-block-complete";
#[cfg(test)]
const CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT: &str = "checkpoint-body-fsync-complete";
#[cfg(test)]
const DESCRIPTOR_WRITE_COMPLETE_FAILPOINT: &str = "descriptor-write-complete";
#[cfg(test)]
const DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT: &str = "descriptor-fsync-complete";
#[cfg(test)]
const STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT: &str = "stale-tail-clear-block-complete";
#[cfg(test)]
const STALE_TAIL_FSYNC_COMPLETE_FAILPOINT: &str = "stale-tail-fsync-complete";

#[cfg(test)]
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
struct AlignedDescriptors([u8; 2 * BLOCK_SIZE]);

#[repr(align(4096))]
struct AlignedRecord([u8; 2 * BLOCK_SIZE]);

#[derive(Clone, Copy, Debug, PartialEq)]
enum CheckpointSlot {
    Green = 0,
    Blue = 1,
}

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

#[derive(Clone, Copy)]
struct Checkpoint {
    slot: CheckpointSlot,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    checkpoint_lsn: u64,
    last_footer_block: u32,
    layout: Layout,
    body_checksum: u64,
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
        #[cfg(test)]
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

    fn publish_checkpoint(&mut self) -> io::Result<()> {
        self.flush()?;
        let body_checksum = self.persist_checkpoint_body()?;
        let mut descriptor = AlignedBlock([0; BLOCK_SIZE]);
        let checkpoint = Checkpoint {
            slot: self.next_checkpoint_slot,
            volume_id: self.volume_id,
            volume_blocks: self.volume_blocks,
            backing_blocks: self.backing_blocks,
            checkpoint_lsn: self.last_lsn,
            last_footer_block: self.last_footer_block,
            layout: layout_for(u64::from(self.volume_blocks) * BLOCK_SIZE as u64)?.1,
            body_checksum,
        };
        encode_checkpoint_descriptor(&mut descriptor.0, &checkpoint);
        let write = opcode::Write::new(
            types::Fd(self.backing.as_raw_fd()),
            descriptor.0.as_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(self.next_checkpoint_slot as u64 * BLOCK_SIZE as u64)
        .build();
        if let Err(error) = submit_exact(&mut self.ring, write, self.last_lsn, BLOCK_SIZE as i32) {
            self.failed = true;
            return Err(error);
        }
        #[cfg(test)]
        pause_at_test_failpoint(DESCRIPTOR_WRITE_COMPLETE_FAILPOINT)?;
        self.flush()?;
        #[cfg(test)]
        pause_at_test_failpoint(DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT)?;
        self.checkpoint_lsn = self.last_lsn;
        self.log_bytes_since_checkpoint = 0;
        self.next_checkpoint_slot = match self.next_checkpoint_slot {
            CheckpointSlot::Green => CheckpointSlot::Blue,
            CheckpointSlot::Blue => CheckpointSlot::Green,
        };
        Ok(())
    }

    fn persist_checkpoint_body(&mut self) -> io::Result<u64> {
        let layout = layout_for(u64::from(self.volume_blocks) * BLOCK_SIZE as u64)?.1;
        let body_start = match self.next_checkpoint_slot {
            CheckpointSlot::Green => layout.green_physical_map_start,
            CheckpointSlot::Blue => layout.blue_physical_map_start,
        };
        let mut hasher = Xxh3::new();
        let mut block = AlignedBlock([0; BLOCK_SIZE]);

        for block_index in 0..layout.physical_map_blocks {
            block.0.fill(0);
            let first = block_index as usize * (BLOCK_SIZE / size_of::<u32>());
            for (bytes, value) in block
                .0
                .chunks_exact_mut(size_of::<u32>())
                .zip(self.physical_blocks[first..].iter())
            {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
            hasher.update(&block.0);
            self.write_checkpoint_body_block(body_start + block_index, &block)?;
            #[cfg(test)]
            pause_at_test_failpoint(&format!(
                "{CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT}-{block_index}"
            ))?;
        }
        for block_index in 0..layout.checksum_map_blocks {
            block.0.fill(0);
            let first = block_index as usize * (BLOCK_SIZE / size_of::<u64>());
            for (bytes, value) in block
                .0
                .chunks_exact_mut(size_of::<u64>())
                .zip(self.checksums[first..].iter())
            {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
            hasher.update(&block.0);
            let body_block_index = layout.physical_map_blocks + block_index;
            self.write_checkpoint_body_block(body_start + body_block_index, &block)?;
            #[cfg(test)]
            pause_at_test_failpoint(&format!(
                "{CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT}-{body_block_index}"
            ))?;
        }
        self.flush()?;
        #[cfg(test)]
        pause_at_test_failpoint(CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT)?;
        Ok(hasher.digest())
    }

    fn write_checkpoint_body_block(
        &mut self,
        physical_block: u32,
        block: &AlignedBlock,
    ) -> io::Result<()> {
        let write = opcode::Write::new(
            types::Fd(self.backing.as_raw_fd()),
            block.0.as_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(u64::from(physical_block) * BLOCK_SIZE as u64)
        .build();
        if let Err(error) = submit_exact(&mut self.ring, write, self.last_lsn, BLOCK_SIZE as i32) {
            self.failed = true;
            return Err(error);
        }
        Ok(())
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
        #[cfg(test)]
        pause_at_test_failpoint(RECORD_WRITE_COMPLETE_FAILPOINT)?;

        self.physical_blocks[lba as usize] = payload_block;
        self.checksums[lba as usize] = checksum;
        self.last_lsn = lsn;
        self.last_footer_block = footer_block;
        self.log_bytes_since_checkpoint += record_bytes;
        #[cfg(test)]
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

fn encode_checkpoint_descriptor(descriptor: &mut [u8], checkpoint: &Checkpoint) {
    descriptor.fill(0);
    descriptor[..4].copy_from_slice(CHECKPOINT_MAGIC);
    descriptor[4] = FORMAT_VERSION;
    descriptor[5] = checkpoint.slot as u8;
    descriptor[8..16].copy_from_slice(&checkpoint.volume_id.to_le_bytes());
    descriptor[16..20].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    descriptor[20..24].copy_from_slice(&checkpoint.volume_blocks.to_le_bytes());
    descriptor[24..32].copy_from_slice(&checkpoint.backing_blocks.to_le_bytes());
    descriptor[32..40].copy_from_slice(&checkpoint.checkpoint_lsn.to_le_bytes());
    descriptor[40..44].copy_from_slice(&checkpoint.last_footer_block.to_le_bytes());
    descriptor[CHECKPOINT_BODY_CHECKSUM_OFFSET..CHECKPOINT_BODY_CHECKSUM_OFFSET + 8]
        .copy_from_slice(&checkpoint.body_checksum.to_le_bytes());

    let checksum = xxh3_64(descriptor);
    descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
}

fn decode_checkpoint(
    descriptor: &mut [u8],
    expected_slot: CheckpointSlot,
    backing_bytes: u64,
) -> Option<Checkpoint> {
    let stored_checksum = read_u64(descriptor, DESCRIPTOR_CHECKSUM_OFFSET);
    descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
    let checksum_valid = xxh3_64(descriptor) == stored_checksum;
    descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&stored_checksum.to_le_bytes());

    let body_checksum = read_u64(descriptor, CHECKPOINT_BODY_CHECKSUM_OFFSET);
    if !checksum_valid
        || &descriptor[..4] != CHECKPOINT_MAGIC
        || descriptor[4] != FORMAT_VERSION
        || descriptor[5] != expected_slot as u8
        || descriptor[6..8] != [0, 0]
        || read_u32(descriptor, 16) != BLOCK_SIZE as u32
        || descriptor
            [CHECKPOINT_BODY_CHECKSUM_OFFSET + size_of::<u64>()..DESCRIPTOR_CHECKSUM_OFFSET]
            .iter()
            .any(|byte| *byte != 0)
    {
        return None;
    }

    let volume_blocks = read_u32(descriptor, 20);
    let (expected_volume_blocks, layout) =
        layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64).ok()?;
    let backing_blocks = backing_blocks_for(&layout, backing_bytes).ok()?;
    let checkpoint_lsn = read_u64(descriptor, 32);
    let last_footer_block = read_u32(descriptor, 40);
    let initial = checkpoint_lsn == 0 && last_footer_block == layout.log_start - 1;
    if volume_blocks != expected_volume_blocks
        || read_u64(descriptor, 24) != backing_blocks
        || last_footer_block < layout.log_start - 1
        || u64::from(last_footer_block) >= backing_blocks
        || ((checkpoint_lsn == 0 || last_footer_block == layout.log_start - 1) && !initial)
        || (initial && body_checksum != 0)
    {
        return None;
    }

    Some(Checkpoint {
        slot: expected_slot,
        volume_id: read_u64(descriptor, 8),
        volume_blocks,
        backing_blocks,
        checkpoint_lsn,
        last_footer_block,
        layout,
        body_checksum,
    })
}

fn load_checkpoint_body(
    ring: &mut Option<IoUring>,
    backing: &File,
    checkpoint: Checkpoint,
) -> io::Result<Option<(Vec<u32>, Vec<u64>)>> {
    let mapping_len = checkpoint.volume_blocks as usize;
    let mut physical_blocks = Vec::new();
    physical_blocks
        .try_reserve_exact(mapping_len)
        .map_err(io::Error::other)?;
    physical_blocks.resize(mapping_len, 0);
    let mut checksums = Vec::new();
    checksums
        .try_reserve_exact(mapping_len)
        .map_err(io::Error::other)?;
    checksums.resize(mapping_len, 0);
    if checkpoint.checkpoint_lsn == 0 {
        return Ok(Some((physical_blocks, checksums)));
    }

    let body_start = match checkpoint.slot {
        CheckpointSlot::Green => checkpoint.layout.green_physical_map_start,
        CheckpointSlot::Blue => checkpoint.layout.blue_physical_map_start,
    };
    let mut hasher = Xxh3::new();
    let mut block = AlignedBlock([0; BLOCK_SIZE]);
    for block_index in 0..checkpoint.layout.physical_map_blocks {
        read_checkpoint_body_block(ring, backing, body_start + block_index, &mut block)?;
        hasher.update(&block.0);
        let first = block_index as usize * (BLOCK_SIZE / size_of::<u32>());
        for (value, bytes) in physical_blocks[first..]
            .iter_mut()
            .zip(block.0.chunks_exact(size_of::<u32>()))
        {
            *value = u32::from_le_bytes(bytes.try_into().unwrap());
        }
    }
    for block_index in 0..checkpoint.layout.checksum_map_blocks {
        read_checkpoint_body_block(
            ring,
            backing,
            body_start + checkpoint.layout.physical_map_blocks + block_index,
            &mut block,
        )?;
        hasher.update(&block.0);
        let first = block_index as usize * (BLOCK_SIZE / size_of::<u64>());
        for (value, bytes) in checksums[first..]
            .iter_mut()
            .zip(block.0.chunks_exact(size_of::<u64>()))
        {
            *value = u64::from_le_bytes(bytes.try_into().unwrap());
        }
    }

    Ok((hasher.digest() == checkpoint.body_checksum).then_some((physical_blocks, checksums)))
}

fn read_checkpoint_body_block(
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
        #[cfg(test)]
        pause_at_test_failpoint(&format!(
            "{STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT}-{block_index}"
        ))?;
    }

    let fsync = opcode::Fsync::new(types::Fd(backing.as_raw_fd())).build();
    submit_exact(ring, fsync, append_block, 0)?;
    #[cfg(test)]
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
        read_checkpoint_body_block(ring, backing, footer_block as u32, &mut footer)?;
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
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs::remove_file;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::FileExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use xxhash_rust::xxh3::xxh3_64_with_seed;

    struct TemporaryBacking(PathBuf);

    impl TemporaryBacking {
        fn new() -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            Ok(Self(std::env::temp_dir().join(format!(
                "block-storage-{}-{nonce}",
                std::process::id()
            ))))
        }

        fn open_new(&self) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_DIRECT)
                .open(&self.0)
        }

        fn create_sized(&self, bytes: u64) -> io::Result<()> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&self.0)?;
            file.set_len(bytes)
        }

        fn reopen(&self) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(&self.0)
        }
    }

    impl Drop for TemporaryBacking {
        fn drop(&mut self) {
            // Test cleanup is best-effort because Drop cannot report errors.
            let _ = remove_file(&self.0);
        }
    }

    const CRASH_TEST_BACKING: &str = "BLOCK_STORAGE_CRASH_TEST_BACKING";
    const CRASH_TEST_MODE: &str = "BLOCK_STORAGE_CRASH_TEST_MODE";
    const ACCEPTANCE_CRASH_TEST_MODE: &str = "acceptance";
    const ACCEPTANCE_CRASH_REPETITIONS: usize = 20;
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    struct CrashTestPlan {
        boundary_indices: Vec<u64>,
        repetitions: usize,
    }

    fn crash_test_plan(mode: Option<&OsStr>, boundary_count: u64) -> io::Result<CrashTestPlan> {
        if mode.is_none() {
            let mut boundary_indices = vec![0, boundary_count / 2, boundary_count - 1];
            boundary_indices.dedup();
            return Ok(CrashTestPlan {
                boundary_indices,
                repetitions: 1,
            });
        }
        if mode == Some(OsStr::new(ACCEPTANCE_CRASH_TEST_MODE)) {
            return Ok(CrashTestPlan {
                boundary_indices: (0..boundary_count).collect(),
                repetitions: ACCEPTANCE_CRASH_REPETITIONS,
            });
        }
        Err(io::Error::other(format!(
            "unsupported {CRASH_TEST_MODE}: {}",
            mode.unwrap_or_default().to_string_lossy()
        )))
    }

    fn configured_crash_test_plan(boundary_count: u64) -> io::Result<CrashTestPlan> {
        crash_test_plan(std::env::var_os(CRASH_TEST_MODE).as_deref(), boundary_count)
    }

    fn kill_and_reap(child: &mut Child) -> io::Result<ExitStatus> {
        match child.kill() {
            Ok(()) => child.wait(),
            Err(kill_error) => child.try_wait()?.ok_or(kill_error),
        }
    }

    fn sigkill_child_at_handshake(
        test_name: &str,
        backing: &Path,
        handshake: &str,
        failpoint: Option<&str>,
    ) -> io::Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(CRASH_TEST_BACKING, backing)
            .stdout(Stdio::piped());
        if let Some(failpoint) = failpoint {
            command.env(TEST_FAILPOINT_ENV, failpoint);
        }
        let mut child = command.spawn()?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                kill_and_reap(&mut child)?;
                return Err(io::Error::other("child stdout unavailable"));
            }
        };
        let expected_handshake = handshake.to_owned();
        let (handshake_sender, handshake_receiver) = mpsc::sync_channel(1);
        let stdout_reader = std::thread::spawn(move || {
            let result = BufReader::new(stdout)
                .lines()
                .find_map(|line| match line {
                    Ok(line) if line == expected_handshake => Some(Ok(())),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .unwrap_or_else(|| Err(io::Error::other("child exited before handshake")));
            handshake_sender.send(result)
        });

        let handshake_result = match handshake_receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(io::Error::other("timed out waiting for child handshake"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("child handshake reader disconnected"))
            }
        };
        let status = kill_and_reap(&mut child)?;
        stdout_reader
            .join()
            .map_err(|_| io::Error::other("child handshake reader panicked"))?
            .map_err(|_| io::Error::other("child handshake receiver disconnected"))?;
        handshake_result?;
        if status.signal() != Some(libc::SIGKILL) {
            return Err(io::Error::other("child was not terminated by SIGKILL"));
        }
        Ok(())
    }

    fn wait_for_child_exit(child: &mut Child) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(wait_error) => {
                    kill_and_reap(child)?;
                    return Err(wait_error);
                }
            }
            if Instant::now() >= deadline {
                kill_and_reap(child)?;
                return Err(io::Error::other("timed out waiting for child exit"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run_unwinding_panic_child(test_name: &str, backing: &Path) -> io::Result<()> {
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", test_name, "--nocapture"])
            .env(CRASH_TEST_BACKING, backing)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("panic child stderr unavailable"))?;
        let stderr_reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            stderr.read_to_end(&mut output).map(|_| output)
        });
        let status_result = wait_for_child_exit(&mut child);
        let stderr = stderr_reader
            .join()
            .map_err(|_| io::Error::other("panic child stderr reader panicked"))??;
        let status = status_result?;
        if status.signal() == Some(libc::SIGKILL) || status.code() != Some(101) {
            return Err(io::Error::other(format!(
                "panic child exited unexpectedly ({status}); stderr:\n{}",
                String::from_utf8_lossy(&stderr)
            )));
        }
        Ok(())
    }

    fn write_exact(
        ring: &mut Option<IoUring>,
        file: &File,
        block: &AlignedBlock,
    ) -> io::Result<()> {
        let entry = opcode::Write::new(
            types::Fd(file.as_raw_fd()),
            block.0.as_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(0)
        .build();
        submit_exact(ring, entry, 1, BLOCK_SIZE as i32)
    }

    fn fsync(ring: &mut Option<IoUring>, file: &File) -> io::Result<()> {
        let entry = opcode::Fsync::new(types::Fd(file.as_raw_fd())).build();
        submit_exact(ring, entry, 2, 0)
    }

    fn read_exact(
        ring: &mut Option<IoUring>,
        file: &File,
        block: &mut AlignedBlock,
    ) -> io::Result<()> {
        let entry = opcode::Read::new(
            types::Fd(file.as_raw_fd()),
            block.0.as_mut_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(0)
        .build();
        submit_exact(ring, entry, 3, BLOCK_SIZE as i32)
    }

    fn write_raw_record_payloads(
        file: &File,
        volume_id: u64,
        lsn: u64,
        previous_footer_block: u32,
        payloads: &[(u32, u8)],
    ) -> io::Result<[u8; BLOCK_SIZE]> {
        assert!(!payloads.is_empty() && payloads.len() <= MAXIMUM_PAYLOAD_BLOCKS);
        let footer_block = previous_footer_block + 1 + payloads.len() as u32;
        let mut footer = [0; BLOCK_SIZE];
        footer[..4].copy_from_slice(FOOTER_MAGIC);
        footer[4] = FORMAT_VERSION;
        footer[5] = RECORD_KIND_WRITE;
        footer[8..16].copy_from_slice(&volume_id.to_le_bytes());
        footer[16..24].copy_from_slice(&lsn.to_le_bytes());
        footer[24..28].copy_from_slice(&previous_footer_block.to_le_bytes());
        footer[28..32].copy_from_slice(&footer_block.to_le_bytes());

        for (index, &(lba, byte)) in payloads.iter().enumerate() {
            let payload_block = previous_footer_block + 1 + index as u32;
            let payload = [byte; BLOCK_SIZE];
            let lba_offset = FOOTER_LBA_OFFSET + index * size_of::<u32>();
            let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + index * size_of::<u64>();
            footer[lba_offset..lba_offset + size_of::<u32>()].copy_from_slice(&lba.to_le_bytes());
            footer[checksum_offset..checksum_offset + size_of::<u64>()].copy_from_slice(
                &payload_checksum(volume_id, lba, payload_block, &payload).to_le_bytes(),
            );
            file.write_all_at(&payload, u64::from(payload_block) * BLOCK_SIZE as u64)?;
        }
        let footer_checksum = xxh3_64(&footer);
        footer[FOOTER_CHECKSUM_OFFSET..].copy_from_slice(&footer_checksum.to_le_bytes());
        file.write_all_at(&footer, u64::from(footer_block) * BLOCK_SIZE as u64)?;
        Ok(footer)
    }

    fn write_raw_record(
        file: &File,
        volume_id: u64,
        lsn: u64,
        previous_footer_block: u32,
        payload_block: u32,
        lba: u32,
        byte: u8,
    ) -> io::Result<[u8; BLOCK_SIZE]> {
        assert_eq!(payload_block, previous_footer_block + 1);
        write_raw_record_payloads(file, volume_id, lsn, previous_footer_block, &[(lba, byte)])
    }

    fn formatted_volume_id(backing: &TemporaryBacking) -> io::Result<u64> {
        let file = OpenOptions::new().read(true).open(&backing.0)?;
        let mut descriptor = [0; BLOCK_SIZE];
        file.read_exact_at(&mut descriptor, 0)?;
        Ok(read_u64(&descriptor, 8))
    }

    fn seal_raw_footer(footer: &mut [u8; BLOCK_SIZE]) {
        footer[FOOTER_CHECKSUM_OFFSET..].fill(0);
        let checksum = xxh3_64(footer);
        footer[FOOTER_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    }

    fn assert_raw_footer_checksum_valid(footer: &[u8; BLOCK_SIZE]) {
        let stored_checksum = read_u64(footer, FOOTER_CHECKSUM_OFFSET);
        let mut checksummed = *footer;
        checksummed[FOOTER_CHECKSUM_OFFSET..].fill(0);
        assert_eq!(xxh3_64(&checksummed), stored_checksum);
    }

    fn raw_footer(
        volume_id: u64,
        lsn: u64,
        previous_footer_block: u32,
        declared_footer_block: u32,
    ) -> [u8; BLOCK_SIZE] {
        let mut footer = [0; BLOCK_SIZE];
        footer[..4].copy_from_slice(FOOTER_MAGIC);
        footer[4] = FORMAT_VERSION;
        footer[5] = RECORD_KIND_WRITE;
        footer[8..16].copy_from_slice(&volume_id.to_le_bytes());
        footer[16..24].copy_from_slice(&lsn.to_le_bytes());
        footer[24..28].copy_from_slice(&previous_footer_block.to_le_bytes());
        footer[28..32].copy_from_slice(&declared_footer_block.to_le_bytes());
        seal_raw_footer(&mut footer);
        footer
    }

    fn prepare_footer_rejection_image(
        backing_blocks: u64,
    ) -> io::Result<(TemporaryBacking, Layout, u64)> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 4_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let volume_id = formatted_volume_id(&backing)?;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        write_raw_record(
            &file,
            volume_id,
            1,
            layout.log_start - 1,
            layout.log_start,
            0,
            0xa5,
        )?;
        file.sync_all()?;
        Ok((backing, layout, volume_id))
    }

    fn verify_recovery_stops_before_invalid_footer(
        backing: &TemporaryBacking,
        clear_start: u32,
    ) -> io::Result<()> {
        let opened = std::panic::catch_unwind(|| open(&backing.0))
            .map_err(|_| io::Error::other("public open panicked on invalid footer"))?;
        let mut volume = opened?;
        assert_eq!(volume.last_lsn, 1);
        assert_eq!(volume.durable_lsn, 1);
        assert_eq!(volume.last_footer_block + 1, clear_start);
        assert_eq!(volume.log_bytes_since_checkpoint, (2 * BLOCK_SIZE) as u64);
        assert_eq!(volume.physical_blocks, [clear_start - 2, 0, 0, 0]);
        assert_eq!(volume.checksums[1..], [0, 0, 0]);

        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0xa5; BLOCK_SIZE]);
        for lba in 1..4 {
            actual.fill(0x5a);
            volume.read_block(lba, &mut actual)?;
            assert_eq!(actual, [0; BLOCK_SIZE]);
        }
        drop(volume);
        Ok(())
    }

    fn assert_zero_blocks(file: &File, start: u32, count: u32) -> io::Result<()> {
        for block in start..start + count {
            let mut bytes = [0xa5; BLOCK_SIZE];
            file.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
            assert_eq!(bytes, [0; BLOCK_SIZE], "physical block {block}");
        }
        Ok(())
    }

    fn verify_rejected_two_payload_footer(
        mutate: impl FnOnce(&mut [u8; BLOCK_SIZE], u64, u32),
    ) -> io::Result<()> {
        let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
        let clear_start = layout.log_start + 2;
        let backing_blocks = u64::from(clear_start) + MAXIMUM_RECORD_BLOCKS + 1;
        let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        file.write_all_at(
            &vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE],
            u64::from(clear_start) * BLOCK_SIZE as u64,
        )?;
        let invalid_footer_block = clear_start + 2;
        let mut footer = write_raw_record_payloads(
            &file,
            volume_id,
            2,
            layout.log_start + 1,
            &[(1, 0x5a), (2, 0xc3)],
        )?;
        assert_eq!(read_u32(&footer, 28), invalid_footer_block);
        mutate(&mut footer, volume_id, clear_start);
        seal_raw_footer(&mut footer);
        assert_raw_footer_checksum_valid(&footer);
        file.write_all_at(&footer, u64::from(invalid_footer_block) * BLOCK_SIZE as u64)?;
        let preserved_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
        file.write_all_at(
            &[0x9d; BLOCK_SIZE],
            u64::from(preserved_block) * BLOCK_SIZE as u64,
        )?;
        file.sync_all()?;
        drop(file);

        verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
        let file = OpenOptions::new().read(true).open(&backing.0)?;
        assert_eq!(file.metadata()?.len(), backing_blocks * BLOCK_SIZE as u64);
        assert_zero_blocks(&file, clear_start, MAXIMUM_RECORD_BLOCKS as u32)?;
        let mut preserved = [0; BLOCK_SIZE];
        file.read_exact_at(
            &mut preserved,
            u64::from(preserved_block) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(preserved, [0x9d; BLOCK_SIZE]);
        Ok(())
    }

    struct InvalidGapFixture {
        backing: TemporaryBacking,
        clear_start: u32,
        later_payload_block: u32,
        later_footer: [u8; BLOCK_SIZE],
        backing_bytes: u64,
    }

    fn prepare_invalid_gap_fixture() -> io::Result<InvalidGapFixture> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let clear_start = layout.log_start + 2;
        let later_payload_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
        let backing_bytes = u64::from(later_payload_block + 2) * BLOCK_SIZE as u64;
        backing.create_sized(backing_bytes)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let volume_id = formatted_volume_id(&backing)?;

        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let first_footer = layout.log_start + 1;
        write_raw_record(
            &file,
            volume_id,
            1,
            layout.log_start - 1,
            layout.log_start,
            0,
            0xa5,
        )?;
        assert_eq!(clear_start, first_footer + 1);
        let stale_window = vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE];
        file.write_all_at(&stale_window, u64::from(clear_start) * BLOCK_SIZE as u64)?;
        let later_footer = write_raw_record(
            &file,
            volume_id,
            3,
            clear_start + MAXIMUM_RECORD_BLOCKS as u32 - 1,
            later_payload_block,
            1,
            0x5a,
        )?;
        file.sync_all()?;

        Ok(InvalidGapFixture {
            backing,
            clear_start,
            later_payload_block,
            later_footer,
            backing_bytes,
        })
    }

    fn verify_cleared_window(fixture: &InvalidGapFixture) -> io::Result<()> {
        let file = OpenOptions::new().read(true).open(&fixture.backing.0)?;
        assert_eq!(file.metadata()?.len(), fixture.backing_bytes);
        for block in fixture.clear_start..fixture.clear_start + MAXIMUM_RECORD_BLOCKS as u32 {
            let mut bytes = [0xa5; BLOCK_SIZE];
            file.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
            assert_eq!(bytes, [0; BLOCK_SIZE]);
        }
        let mut later_payload = [0; BLOCK_SIZE];
        file.read_exact_at(
            &mut later_payload,
            u64::from(fixture.later_payload_block) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(later_payload, [0x5a; BLOCK_SIZE]);
        let mut later_footer = [0; BLOCK_SIZE];
        file.read_exact_at(
            &mut later_footer,
            u64::from(fixture.later_payload_block + 1) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(later_footer, fixture.later_footer);
        Ok(())
    }

    fn verify_invalid_gap_recovery(fixture: &InvalidGapFixture) -> io::Result<()> {
        let mut volume = open(&fixture.backing.0)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0xa5; BLOCK_SIZE]);
        volume.read_block(1, &mut actual)?;
        assert_eq!(actual, [0; BLOCK_SIZE]);
        assert_eq!(volume.last_lsn, 1);
        assert_eq!(volume.last_footer_block + 1, fixture.clear_start);
        drop(volume);
        verify_cleared_window(fixture)
    }

    #[test]
    fn xxh3_matches_persistent_format_vectors() {
        assert_eq!(xxh3_64(b""), 0x2d06_8005_38d3_94c2);
        assert_eq!(xxh3_64(&[0; BLOCK_SIZE]), 0x93d7_6fe1_48c6_89ba);

        let mut payload_input = [0xa5; BLOCK_SIZE + 8];
        payload_input[..4].copy_from_slice(&0x0102_0304_u32.to_le_bytes());
        payload_input[4..8].copy_from_slice(&0x0506_0708_u32.to_le_bytes());
        assert_eq!(
            xxh3_64_with_seed(&payload_input, 0x1122_3344_5566_7788),
            0x4681_58e2_0c4c_72c4
        );
    }

    #[test]
    fn direct_io_uring_write_survives_fsync_and_reopen() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let mut ring = Some(IoUring::new(4)?);
        let written = AlignedBlock([0xa5; BLOCK_SIZE]);

        {
            let file = backing.open_new()?;
            write_exact(&mut ring, &file, &written)?;
            fsync(&mut ring, &file)?;
        }

        let mut read = AlignedBlock([0; BLOCK_SIZE]);
        {
            let file = backing.reopen()?;
            read_exact(&mut ring, &file, &mut read)?;
        }

        assert_eq!(written.0, read.0);
        Ok(())
    }

    #[test]
    fn waits_when_submission_succeeds_before_completion_exists() -> io::Result<()> {
        let event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if event_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let event = unsafe { File::from_raw_fd(event_fd) };
        let mut ring = IoUring::new(2)?;
        let poll = opcode::PollAdd::new(types::Fd(event_fd), libc::POLLIN as u32)
            .build()
            .user_data(7);
        unsafe {
            ring.submission()
                .push(&poll)
                .map_err(|_| io::Error::other("io_uring submission queue is full"))?;
        }
        let mut ring = Some(ring);
        let mut calls = 0;
        let mut write_result = 0;
        let result = wait_exact_with(
            &mut ring,
            7,
            libc::POLLIN.into(),
            |ring| -> io::Result<usize> {
                calls += 1;
                if calls == 1 {
                    assert_eq!(ring.submit()?, 1);
                    return Ok(1);
                }
                let value = 1_u64;
                write_result =
                    unsafe { libc::write(event_fd, (&raw const value).cast(), size_of::<u64>()) };
                assert_eq!(ring.submit_and_wait(1)?, 0);
                Ok(0)
            },
        );

        assert_eq!(calls, 2);
        assert_eq!(write_result, size_of::<u64>() as isize);
        drop(event);
        result
    }

    #[test]
    fn destroys_ring_before_returning_permanent_wait_error() -> io::Result<()> {
        let mut ring = Some(IoUring::new(2)?);
        let error = wait_exact_with(&mut ring, 7, 0, |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();

        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(ring.is_none());
        Ok(())
    }

    #[test]
    fn multiple_lbas_read_correctly_before_and_after_reopen() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let expected = [[0xa5; BLOCK_SIZE], [0x5a; BLOCK_SIZE]];
        let mut actual = [0; BLOCK_SIZE];
        let mut volume = open(&backing.0)?;
        for (lba, block) in expected.iter().enumerate() {
            volume.write_block(lba as u32, block)?;
        }
        for (lba, block) in expected.iter().enumerate() {
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, block);
        }
        volume.close()?;

        let mut volume = open(&backing.0)?;
        for (lba, block) in expected.iter().enumerate() {
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, block);
        }
        volume.close()
    }

    #[test]
    fn repeated_overwrite_returns_only_latest_value() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;

        let values = [[0xa5; BLOCK_SIZE], [0x5a; BLOCK_SIZE], [0xc3; BLOCK_SIZE]];
        for expected in values {
            volume.write_block(0, &expected)?;
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(0, &mut actual)?;
            assert_eq!(actual, expected);
        }
        volume.flush()?;
        volume.close()?;

        let mut volume = open(&backing.0)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, values[2]);
        volume.close()
    }

    #[test]
    fn overwriting_lba_with_zeroes_returns_zeroes_before_and_after_reopen() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;

        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.write_block(0, &[0; BLOCK_SIZE])?;
        let mut actual = [0xa5; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0; BLOCK_SIZE]);
        volume.close()?;

        let mut volume = open(&backing.0)?;
        actual.fill(0xa5);
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0; BLOCK_SIZE]);
        volume.close()
    }

    #[test]
    fn raw_footer_linkage_and_lsns_are_contiguous() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;
        let initial_footer_block = volume.last_footer_block;

        for byte in [0xa5, 0x5a, 0xc3] {
            volume.write_block(0, &[byte; BLOCK_SIZE])?;
        }

        let file = File::open(&backing.0)?;
        let mut previous_footer_block = initial_footer_block;
        for lsn in 1..=3 {
            let footer_block = previous_footer_block + 2;
            let mut footer = [0; BLOCK_SIZE];
            file.read_exact_at(&mut footer, u64::from(footer_block) * BLOCK_SIZE as u64)?;
            assert_eq!(read_u64(&footer, 16), lsn);
            assert_eq!(read_u32(&footer, 24), previous_footer_block);
            assert_eq!(read_u32(&footer, 28), footer_block);
            previous_footer_block = footer_block;
        }
        Ok(())
    }

    #[test]
    fn clean_checkpoint_maps_each_lba_to_latest_payload_and_checksum() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let mut volume = open(&backing.0)?;
        let volume_id = volume.volume_id;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
        volume.write_block(0, &[0xc3; BLOCK_SIZE])?;
        volume.close()?;

        let file = File::open(&backing.0)?;
        let mut body = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut body,
            u64::from(layout.blue_physical_map_start) * BLOCK_SIZE as u64,
        )?;
        let expected = [
            (layout.log_start + 4, [0xc3; BLOCK_SIZE]),
            (layout.log_start + 2, [0x5a; BLOCK_SIZE]),
        ];
        for (lba, (expected_physical_block, expected_payload)) in expected.iter().enumerate() {
            let physical_block = read_u32(&body, lba * size_of::<u32>());
            assert_eq!(physical_block, *expected_physical_block);

            let mut payload = [0; BLOCK_SIZE];
            file.read_exact_at(&mut payload, u64::from(physical_block) * BLOCK_SIZE as u64)?;
            assert_eq!(payload, *expected_payload);

            let mut checksum_input = [0; BLOCK_SIZE + 8];
            checksum_input[..4].copy_from_slice(&(lba as u32).to_le_bytes());
            checksum_input[4..8].copy_from_slice(&physical_block.to_le_bytes());
            checksum_input[8..].copy_from_slice(&payload);
            assert_eq!(
                read_u64(&body, BLOCK_SIZE + lba * size_of::<u64>()),
                xxh3_64_with_seed(&checksum_input, volume_id)
            );
        }
        Ok(())
    }

    #[test]
    fn log_full_write_leaves_last_successful_value_readable_after_reopen() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;

        let expected = [0xa5; BLOCK_SIZE];
        let mut volume = open(&backing.0)?;
        volume.write_block(0, &expected)?;
        let physical_blocks = volume.physical_blocks.clone();
        let checksums = volume.checksums.clone();
        let cursors = (
            volume.last_lsn,
            volume.durable_lsn,
            volume.checkpoint_lsn,
            volume.last_footer_block,
            volume.log_bytes_since_checkpoint,
            volume.next_checkpoint_slot,
            volume.failed,
        );
        let file = File::open(&backing.0)?;
        let mut log_bytes = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut log_bytes,
            u64::from(layout.log_start) * BLOCK_SIZE as u64,
        )?;

        assert_eq!(
            volume
                .write_block(0, &[0x5a; BLOCK_SIZE])
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );

        assert_eq!(volume.physical_blocks, physical_blocks);
        assert_eq!(volume.checksums, checksums);
        assert_eq!(
            (
                volume.last_lsn,
                volume.durable_lsn,
                volume.checkpoint_lsn,
                volume.last_footer_block,
                volume.log_bytes_since_checkpoint,
                volume.next_checkpoint_slot,
                volume.failed,
            ),
            cursors
        );
        let mut unchanged_log_bytes = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut unchanged_log_bytes,
            u64::from(layout.log_start) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(unchanged_log_bytes, log_bytes);

        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, expected);
        volume.close()?;

        let mut volume = open(&backing.0)?;
        actual.fill(0);
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, expected);
        volume.close()
    }

    #[test]
    fn out_of_range_write_leaves_mapping_and_cursors_unchanged() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;

        let expected = [0xa5; BLOCK_SIZE];
        volume.write_block(0, &expected)?;
        volume.flush()?;
        let physical_blocks = volume.physical_blocks.clone();
        let checksums = volume.checksums.clone();
        let cursors = (
            volume.last_lsn,
            volume.durable_lsn,
            volume.checkpoint_lsn,
            volume.last_footer_block,
            volume.next_checkpoint_slot,
            volume.log_bytes_since_checkpoint,
            volume.failed,
        );

        let error = volume
            .write_block(volume.volume_blocks, &[0x5a; BLOCK_SIZE])
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(volume.physical_blocks, physical_blocks);
        assert_eq!(volume.checksums, checksums);
        assert_eq!(
            (
                volume.last_lsn,
                volume.durable_lsn,
                volume.checkpoint_lsn,
                volume.last_footer_block,
                volume.next_checkpoint_slot,
                volume.log_bytes_since_checkpoint,
                volume.failed,
            ),
            cursors
        );
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn write_block_appends_complete_raw_record_and_publishes_mapping() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;

        let lba = 1_u32;
        let payload = [0xa5; BLOCK_SIZE];
        let previous_footer_block = volume.last_footer_block;
        let payload_block = previous_footer_block + 1;
        let footer_block = payload_block + 1;
        volume.write_block(lba, &payload)?;

        let mut record = [0; 2 * BLOCK_SIZE];
        File::open(&backing.0)?
            .read_exact_at(&mut record, u64::from(payload_block) * BLOCK_SIZE as u64)?;
        assert_eq!(&record[..BLOCK_SIZE], &payload);
        let footer = &mut record[BLOCK_SIZE..];
        assert_eq!(&footer[..4], FOOTER_MAGIC);
        assert_eq!(&footer[4..8], &[FORMAT_VERSION, RECORD_KIND_WRITE, 0, 0]);
        assert_eq!(read_u64(footer, 8), volume.volume_id);
        assert_eq!(read_u64(footer, 16), 1);
        assert_eq!(read_u32(footer, 24), previous_footer_block);
        assert_eq!(read_u32(footer, 28), footer_block);
        assert_eq!(read_u32(footer, FOOTER_LBA_OFFSET), lba);
        let mut checksum_input = [0; BLOCK_SIZE + 8];
        checksum_input[..4].copy_from_slice(&lba.to_le_bytes());
        checksum_input[4..8].copy_from_slice(&payload_block.to_le_bytes());
        checksum_input[8..].copy_from_slice(&payload);
        let expected_payload_checksum = xxh3_64_with_seed(&checksum_input, volume.volume_id);
        assert_eq!(
            read_u64(footer, FOOTER_PAYLOAD_CHECKSUM_OFFSET),
            expected_payload_checksum
        );
        assert!(
            footer[FOOTER_LBA_OFFSET + 4..FOOTER_PAYLOAD_CHECKSUM_OFFSET]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert!(
            footer[FOOTER_PAYLOAD_CHECKSUM_OFFSET + 8..FOOTER_CHECKSUM_OFFSET]
                .iter()
                .all(|byte| *byte == 0)
        );
        let stored_footer_checksum = read_u64(footer, FOOTER_CHECKSUM_OFFSET);
        footer[FOOTER_CHECKSUM_OFFSET..].fill(0);
        assert_eq!(xxh3_64(footer), stored_footer_checksum);

        assert_eq!(volume.physical_blocks, [0, payload_block]);
        assert_eq!(volume.checksums, [0, expected_payload_checksum]);
        assert_eq!(volume.last_lsn, 1);
        assert_eq!(volume.durable_lsn, 0);
        assert_eq!(volume.last_footer_block, footer_block);
        assert_eq!(volume.log_bytes_since_checkpoint, (2 * BLOCK_SIZE) as u64);
        Ok(())
    }

    #[test]
    fn failed_write_is_not_published() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;
        volume.backing = backing.reopen()?;

        let initial_footer_block = volume.last_footer_block;
        let error = volume.write_block(0, &[0xa5; BLOCK_SIZE]).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        assert_eq!(volume.physical_blocks, [0]);
        assert_eq!(volume.checksums, [0]);
        assert_eq!(volume.last_lsn, 0);
        assert_eq!(volume.durable_lsn, 0);
        assert_eq!(volume.last_footer_block, initial_footer_block);
        assert_eq!(volume.log_bytes_since_checkpoint, 0);

        let mut block = [0x5a; BLOCK_SIZE];
        assert_eq!(
            volume.read_block(0, &mut block).unwrap_err().to_string(),
            "volume failed"
        );
        assert_eq!(block, [0x5a; BLOCK_SIZE]);
        assert_eq!(
            volume
                .write_block(0, &[0; BLOCK_SIZE])
                .unwrap_err()
                .to_string(),
            "volume failed"
        );
        assert_eq!(volume.flush().unwrap_err().to_string(), "volume failed");

        assert_eq!(volume.close().unwrap_err().to_string(), "volume failed");
        Ok(())
    }

    #[test]
    fn public_operations_complete_v0_4_acceptance() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let mut expected = [0; BLOCK_SIZE];
        for (index, byte) in expected.iter_mut().enumerate() {
            *byte = index as u8;
        }
        {
            let mut volume = open(&backing.0)?;
            let mut actual = [0xa5; BLOCK_SIZE];
            volume.read_block(0, &mut actual)?;
            assert_eq!(actual, [0; BLOCK_SIZE]);
            assert_eq!(
                volume
                    .read_block(volume_blocks, &mut actual)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );

            volume.write_block(1, &expected)?;
            volume.read_block(1, &mut actual)?;
            assert_eq!(actual, expected);
            volume.flush()?;
            volume.close()?;
        }
        {
            let mut volume = open(&backing.0)?;
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(1, &mut actual)?;
            assert_eq!(actual, expected);
            volume.close()?;
        }

        let file = OpenOptions::new().write(true).open(&backing.0)?;
        file.write_all_at(
            &[expected[0] ^ 0xff],
            u64::from(layout.log_start) * BLOCK_SIZE as u64,
        )?;
        file.sync_all()?;
        let mut volume = open(&backing.0)?;
        let mut actual = [0x5a; BLOCK_SIZE];
        assert_eq!(
            volume.read_block(1, &mut actual).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(actual, [0x5a; BLOCK_SIZE]);
        Ok(())
    }

    #[test]
    fn recover_one_flushed_write_after_sigkill_without_close() -> io::Result<()> {
        const HANDSHAKE: &str = "write-flushed";

        if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
            let mut volume = open(PathBuf::from(backing))?;
            volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
            volume.flush()?;
            println!("{HANDSHAKE}");
            io::stdout().flush()?;
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;

        sigkill_child_at_handshake(
            "tests::recover_one_flushed_write_after_sigkill_without_close",
            &backing.0,
            HANDSHAKE,
            None,
        )?;

        let mut volume = open(&backing.0)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0xa5; BLOCK_SIZE]);
        volume.close()
    }

    #[test]
    fn recover_multiple_flushed_writes_and_overwrites_after_sigkill() -> io::Result<()> {
        const HANDSHAKE: &str = "all-writes-flushed";

        let writes = [
            (0, 0xa5),
            (1, 0x5a),
            (0, 0xc3),
            (2, 0x3c),
            (0, 0xd4),
            (1, 0xe7),
        ];
        if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
            let mut volume = open(PathBuf::from(backing))?;
            for (lba, byte) in writes {
                volume.write_block(lba, &[byte; BLOCK_SIZE])?;
            }
            volume.flush()?;
            println!("{HANDSHAKE}");
            io::stdout().flush()?;
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        let backing = TemporaryBacking::new()?;
        let volume_blocks = 3_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        sigkill_child_at_handshake(
            "tests::recover_multiple_flushed_writes_and_overwrites_after_sigkill",
            &backing.0,
            HANDSHAKE,
            None,
        )?;

        let expected = [[0xd4; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
        let mut volume = open(&backing.0)?;
        for (lba, expected) in expected.iter().enumerate() {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, expected);
        }
        volume.close()
    }

    #[test]
    fn recover_after_uncaught_unwinding_panic_without_close() -> io::Result<()> {
        let writes = [(0, 0xa5), (1, 0x5a), (0, 0xc3), (2, 0x3c), (1, 0xe7)];
        if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
            let mut volume = open(PathBuf::from(backing))?;
            for (lba, byte) in writes {
                volume.write_block(lba, &[byte; BLOCK_SIZE])?;
            }
            volume.flush()?;
            panic!("intentional uncaught panic after flushing test values");
        }

        let backing = TemporaryBacking::new()?;
        let volume_blocks = 3_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        run_unwinding_panic_child(
            "tests::recover_after_uncaught_unwinding_panic_without_close",
            &backing.0,
        )?;

        let expected = [[0xc3; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
        let mut volume = open(&backing.0)?;
        for (lba, expected) in expected.iter().enumerate() {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, expected);
        }
        volume.close()
    }

    fn recover_after_write_crash(
        test_name: &str,
        failpoint: &str,
        flush_write: bool,
        require_new_state: bool,
    ) -> io::Result<()> {
        let old = [0xa5; BLOCK_SIZE];
        let new = [0x5a; BLOCK_SIZE];
        if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
            let mut volume = open(PathBuf::from(backing))?;
            volume.write_block(0, &new)?;
            if flush_write {
                volume.flush()?;
            }
            return Err(io::Error::other("test failpoint did not pause child"));
        }

        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;
        volume.write_block(0, &old)?;
        volume.close()?;

        sigkill_child_at_handshake(test_name, &backing.0, failpoint, Some(failpoint))?;

        let mut volume = open(&backing.0)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        if require_new_state {
            assert_eq!(actual, new);
        } else {
            assert!(actual == old || actual == new);
        }
        volume.close()
    }

    #[test]
    fn recover_after_kill_at_record_write_boundary() -> io::Result<()> {
        recover_after_write_crash(
            "tests::recover_after_kill_at_record_write_boundary",
            RECORD_WRITE_COMPLETE_FAILPOINT,
            false,
            false,
        )
    }

    #[test]
    fn recover_after_kill_at_mapping_publication_boundary() -> io::Result<()> {
        recover_after_write_crash(
            "tests::recover_after_kill_at_mapping_publication_boundary",
            MAPPING_PUBLISHED_FAILPOINT,
            false,
            false,
        )
    }

    #[test]
    fn recover_after_kill_at_log_fsync_boundary() -> io::Result<()> {
        recover_after_write_crash(
            "tests::recover_after_kill_at_log_fsync_boundary",
            LOG_FSYNC_COMPLETE_FAILPOINT,
            true,
            true,
        )
    }

    fn recover_after_checkpoint_crash(
        test_name: &str,
        failpoint: &str,
        expected_checkpoint_lsn: u64,
    ) -> io::Result<()> {
        let writes = [(0, 0x5a), (1, 0xc3), (2, 0x3c), (1, 0xe7)];
        if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
            let mut volume = open(PathBuf::from(backing))?;
            for (lba, byte) in writes {
                volume.write_block(lba, &[byte; BLOCK_SIZE])?;
            }
            volume.flush()?;
            volume.close()?;
            return Err(io::Error::other("test failpoint did not pause child"));
        }

        let backing = TemporaryBacking::new()?;
        let volume_blocks = 1025_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        assert!(layout.physical_map_blocks + layout.checksum_map_blocks > 1);
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.close()?;

        sigkill_child_at_handshake(test_name, &backing.0, failpoint, Some(failpoint))?;

        let expected = [[0x5a; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
        let mut volume = open(&backing.0)?;
        assert_eq!(volume.checkpoint_lsn, expected_checkpoint_lsn);
        assert_eq!(volume.last_lsn, 5);
        for (lba, expected) in expected.iter().enumerate() {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, expected);
        }
        Ok(())
    }

    #[test]
    fn crash_test_modes_select_smoke_and_exhaustive_work() -> io::Result<()> {
        let smoke = crash_test_plan(None, 339)?;
        assert_eq!(smoke.boundary_indices, [0, 169, 338]);
        assert_eq!(smoke.repetitions, 1);

        let acceptance = crash_test_plan(Some(OsStr::new(ACCEPTANCE_CRASH_TEST_MODE)), 339)?;
        assert_eq!(acceptance.boundary_indices, (0..339).collect::<Vec<_>>());
        assert_eq!(acceptance.repetitions, 20);
        assert!(crash_test_plan(Some(OsStr::new("invalid")), 339).is_err());
        Ok(())
    }

    #[test]
    fn recover_after_each_checkpoint_body_block_boundary() -> io::Result<()> {
        let (_, layout) = layout_for(1025 * BLOCK_SIZE as u64)?;
        let body_blocks = layout.physical_map_blocks + layout.checksum_map_blocks;
        let plan = configured_crash_test_plan(u64::from(body_blocks))?;
        for block_index in plan.boundary_indices {
            let failpoint = format!("{CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT}-{block_index}");
            for _ in 0..plan.repetitions {
                recover_after_checkpoint_crash(
                    "tests::recover_after_each_checkpoint_body_block_boundary",
                    &failpoint,
                    1,
                )?;
            }
        }
        Ok(())
    }

    #[test]
    fn recover_after_checkpoint_body_fsync_boundary() -> io::Result<()> {
        for _ in 0..configured_crash_test_plan(1)?.repetitions {
            recover_after_checkpoint_crash(
                "tests::recover_after_checkpoint_body_fsync_boundary",
                CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT,
                1,
            )?;
        }
        Ok(())
    }

    #[test]
    fn recover_after_descriptor_write_boundary() -> io::Result<()> {
        for _ in 0..configured_crash_test_plan(1)?.repetitions {
            recover_after_checkpoint_crash(
                "tests::recover_after_descriptor_write_boundary",
                DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
                5,
            )?;
        }
        Ok(())
    }

    #[test]
    fn recover_after_descriptor_fsync_boundary() -> io::Result<()> {
        for _ in 0..configured_crash_test_plan(1)?.repetitions {
            recover_after_checkpoint_crash(
                "tests::recover_after_descriptor_fsync_boundary",
                DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
                5,
            )?;
        }
        Ok(())
    }

    struct CheckpointFallbackFixture {
        backing: TemporaryBacking,
        newest: Checkpoint,
        older_lsn: u64,
        last_lsn: u64,
        expected: [[u8; BLOCK_SIZE]; 3],
    }

    fn prepare_checkpoint_fallback_fixture() -> io::Result<CheckpointFallbackFixture> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 3_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let backing_bytes = u64::from(layout.log_start + 64) * BLOCK_SIZE as u64;
        backing.create_sized(backing_bytes)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
        volume.close()?;

        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xc3; BLOCK_SIZE])?;
        volume.write_block(2, &[0x3c; BLOCK_SIZE])?;
        volume.close()?;

        let expected = [[0xd4; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x7e; BLOCK_SIZE]];
        let mut volume = open(&backing.0)?;
        for (lba, value) in expected.iter().enumerate() {
            volume.write_block(lba as u32, value)?;
        }
        volume.flush()?;
        let last_lsn = volume.last_lsn;
        drop(volume);

        let file = OpenOptions::new().read(true).open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        let (green_bytes, blue_bytes) = descriptors.split_at_mut(BLOCK_SIZE);
        let green = decode_checkpoint(green_bytes, CheckpointSlot::Green, backing_bytes)
            .expect("green checkpoint descriptor must be valid before corruption");
        let blue = decode_checkpoint(blue_bytes, CheckpointSlot::Blue, backing_bytes)
            .expect("blue checkpoint descriptor must be valid before corruption");
        assert!(green.checkpoint_lsn > blue.checkpoint_lsn);
        assert_eq!(last_lsn, green.checkpoint_lsn + expected.len() as u64);

        for checkpoint in [green, blue] {
            let body_start = match checkpoint.slot {
                CheckpointSlot::Green => checkpoint.layout.green_physical_map_start,
                CheckpointSlot::Blue => checkpoint.layout.blue_physical_map_start,
            };
            let body_blocks =
                checkpoint.layout.physical_map_blocks + checkpoint.layout.checksum_map_blocks;
            let mut body = vec![0; body_blocks as usize * BLOCK_SIZE];
            file.read_exact_at(&mut body, u64::from(body_start) * BLOCK_SIZE as u64)?;
            assert_eq!(xxh3_64(&body), checkpoint.body_checksum);
        }

        Ok(CheckpointFallbackFixture {
            backing,
            newest: green,
            older_lsn: blue.checkpoint_lsn,
            last_lsn,
            expected,
        })
    }

    fn verify_checkpoint_fallback(fixture: &CheckpointFallbackFixture) -> io::Result<()> {
        let opened = std::panic::catch_unwind(|| open(&fixture.backing.0))
            .map_err(|_| io::Error::other("public open panicked on checkpoint corruption"))?;
        let mut volume = opened?;
        assert_eq!(volume.checkpoint_lsn, fixture.older_lsn);
        assert_eq!(volume.last_lsn, fixture.last_lsn);
        for (lba, expected) in fixture.expected.iter().enumerate() {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, expected);
        }
        volume.close()
    }

    #[test]
    fn falls_back_from_corrupted_newest_descriptor_and_replays_tail() -> io::Result<()> {
        let fixture = prepare_checkpoint_fallback_fixture()?;
        let descriptor_offset = fixture.newest.slot as u64 * BLOCK_SIZE as u64 + 100;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.backing.0)?;
        let mut byte = [0; 1];
        file.read_exact_at(&mut byte, descriptor_offset)?;
        byte[0] ^= 0xff;
        file.write_all_at(&byte, descriptor_offset)?;
        file.sync_all()?;
        drop(file);

        verify_checkpoint_fallback(&fixture)
    }

    #[test]
    fn falls_back_from_corrupted_newest_checkpoint_body_and_replays_tail() -> io::Result<()> {
        let fixture = prepare_checkpoint_fallback_fixture()?;
        let body_start = match fixture.newest.slot {
            CheckpointSlot::Green => fixture.newest.layout.green_physical_map_start,
            CheckpointSlot::Blue => fixture.newest.layout.blue_physical_map_start,
        };
        let body_offset = u64::from(body_start) * BLOCK_SIZE as u64 + 100;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.backing.0)?;
        let mut byte = [0; 1];
        file.read_exact_at(&mut byte, body_offset)?;
        byte[0] ^= 0xff;
        file.write_all_at(&byte, body_offset)?;
        file.sync_all()?;
        drop(file);

        verify_checkpoint_fallback(&fixture)
    }

    #[test]
    fn rejects_two_unusable_checkpoint_roots_without_panic_or_mutation() -> io::Result<()> {
        let fixture = prepare_checkpoint_fallback_fixture()?;
        let newest_descriptor_offset = fixture.newest.slot as u64 * BLOCK_SIZE as u64 + 100;
        let older_body_start = match fixture.newest.slot {
            CheckpointSlot::Green => fixture.newest.layout.blue_physical_map_start,
            CheckpointSlot::Blue => fixture.newest.layout.green_physical_map_start,
        };
        let older_body_offset = u64::from(older_body_start) * BLOCK_SIZE as u64 + 100;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.backing.0)?;
        for offset in [newest_descriptor_offset, older_body_offset] {
            let mut byte = [0; 1];
            file.read_exact_at(&mut byte, offset)?;
            byte[0] ^= 0xff;
            file.write_all_at(&byte, offset)?;
        }
        file.sync_all()?;
        drop(file);

        let corrupted = std::fs::read(&fixture.backing.0)?;
        for _ in 0..2 {
            let opened = std::panic::catch_unwind(|| open(&fixture.backing.0))
                .map_err(|_| io::Error::other("public open panicked on checkpoint corruption"))?;
            let error = match opened {
                Ok(_) => return Err(io::Error::other("corrupted checkpoint roots were served")),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&fixture.backing.0)?, corrupted);
        }
        Ok(())
    }

    #[test]
    fn reconstructs_every_recovery_cursor_from_replayed_state() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 4_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let backing_blocks = u64::from(layout.log_start) + 64;
        let backing_bytes = backing_blocks * BLOCK_SIZE as u64;
        backing.create_sized(backing_bytes)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.write_block(1, &[0x5a; BLOCK_SIZE])?;
        volume.close()?;

        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        let (green_bytes, blue_bytes) = descriptors.split_at_mut(BLOCK_SIZE);
        let green = decode_checkpoint(green_bytes, CheckpointSlot::Green, backing_bytes)
            .expect("green checkpoint descriptor must remain valid");
        let blue = decode_checkpoint(blue_bytes, CheckpointSlot::Blue, backing_bytes)
            .expect("blue checkpoint descriptor must remain valid");
        let selected_checkpoint = [green, blue]
            .into_iter()
            .max_by_key(|checkpoint| checkpoint.checkpoint_lsn)
            .expect("formatted volume has checkpoint roots");
        assert_eq!(selected_checkpoint.checkpoint_lsn, 2);

        let checkpoint_footer = selected_checkpoint.last_footer_block;
        write_raw_record_payloads(
            &file,
            selected_checkpoint.volume_id,
            3,
            checkpoint_footer,
            &[(0, 0xc3)],
        )?;
        let second_footer = checkpoint_footer + 2;
        write_raw_record_payloads(
            &file,
            selected_checkpoint.volume_id,
            4,
            second_footer,
            &[(2, 0x3c), (1, 0xe7), (3, 0x7e)],
        )?;
        let third_footer = second_footer + 4;
        write_raw_record_payloads(
            &file,
            selected_checkpoint.volume_id,
            5,
            third_footer,
            &[(2, 0xd4)],
        )?;
        file.sync_all()?;
        drop(file);

        let final_footer = third_footer + 2;
        let append_block = final_footer + 1;
        let replayed_blocks = u64::from(final_footer - checkpoint_footer);
        let expected = [
            (checkpoint_footer + 1, 0xc3),
            (second_footer + 2, 0xe7),
            (third_footer + 1, 0xd4),
            (second_footer + 3, 0x7e),
        ];

        let mut volume = open(&backing.0)?;
        assert_eq!(volume.last_lsn, 5);
        assert_eq!(volume.last_footer_block, final_footer);
        assert_eq!(volume.durable_lsn, 5);
        assert_eq!(volume.checkpoint_lsn, selected_checkpoint.checkpoint_lsn);
        assert_eq!(
            volume.log_bytes_since_checkpoint,
            replayed_blocks * BLOCK_SIZE as u64
        );
        assert_eq!(volume.last_footer_block + 1, append_block);
        assert_eq!(
            volume.next_checkpoint_slot,
            match selected_checkpoint.slot {
                CheckpointSlot::Green => CheckpointSlot::Blue,
                CheckpointSlot::Blue => CheckpointSlot::Green,
            }
        );

        for (lba, &(physical_block, byte)) in expected.iter().enumerate() {
            let payload = [byte; BLOCK_SIZE];
            assert_eq!(volume.physical_blocks[lba], physical_block);
            assert_eq!(
                volume.checksums[lba],
                payload_checksum(
                    selected_checkpoint.volume_id,
                    lba as u32,
                    physical_block,
                    &payload,
                )
            );
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(actual, payload);
        }
        Ok(())
    }

    #[test]
    fn recovery_accepts_every_format_one_payload_count() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = MAXIMUM_PAYLOAD_BLOCKS as u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let record_blocks = (1..=MAXIMUM_PAYLOAD_BLOCKS as u64)
            .map(|payload_blocks| payload_blocks + 1)
            .sum::<u64>();
        let backing_blocks = u64::from(layout.log_start) + record_blocks + MAXIMUM_RECORD_BLOCKS;
        backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let volume_id = formatted_volume_id(&backing)?;

        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let mut previous_footer_block = layout.log_start - 1;
        for payload_count in 1..=MAXIMUM_PAYLOAD_BLOCKS {
            let payloads = (0..payload_count)
                .map(|index| (index as u32, ((payload_count + index) % 251 + 1) as u8))
                .collect::<Vec<_>>();
            write_raw_record_payloads(
                &file,
                volume_id,
                payload_count as u64,
                previous_footer_block,
                &payloads,
            )?;
            previous_footer_block += payload_count as u32 + 1;
        }
        file.sync_all()?;
        drop(file);

        let mut volume = open(&backing.0)?;
        assert_eq!(volume.last_lsn, MAXIMUM_PAYLOAD_BLOCKS as u64);
        assert_eq!(volume.last_footer_block, previous_footer_block);
        assert_eq!(
            volume.log_bytes_since_checkpoint,
            record_blocks * BLOCK_SIZE as u64
        );
        for lba in 0..volume_blocks {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba, &mut actual)?;
            let byte = ((MAXIMUM_PAYLOAD_BLOCKS + lba as usize) % 251 + 1) as u8;
            assert_eq!(actual, [byte; BLOCK_SIZE]);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_footer_with_zero_payloads() -> io::Result<()> {
        let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
        let clear_start = layout.log_start + 2;
        let backing_blocks = u64::from(clear_start) + MAXIMUM_RECORD_BLOCKS + 1;
        let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        file.write_all_at(
            &vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE],
            u64::from(clear_start) * BLOCK_SIZE as u64,
        )?;
        let mut footer = raw_footer(volume_id, 2, layout.log_start + 1, clear_start);
        assert_raw_footer_checksum_valid(&footer);
        let replay_cursor = Checkpoint {
            slot: CheckpointSlot::Green,
            volume_id,
            volume_blocks: 4,
            backing_blocks,
            checkpoint_lsn: 1,
            last_footer_block: layout.log_start + 1,
            layout,
            body_checksum: 0,
        };
        assert!(decode_write_tail_footer(&mut footer, replay_cursor, clear_start).is_none());
        file.write_all_at(&footer, u64::from(clear_start) * BLOCK_SIZE as u64)?;
        let preserved_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
        file.write_all_at(
            &[0x9d; BLOCK_SIZE],
            u64::from(preserved_block) * BLOCK_SIZE as u64,
        )?;
        file.sync_all()?;
        drop(file);

        verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
        let file = OpenOptions::new().read(true).open(&backing.0)?;
        assert_zero_blocks(&file, clear_start, MAXIMUM_RECORD_BLOCKS as u32)?;
        let mut preserved = [0; BLOCK_SIZE];
        file.read_exact_at(
            &mut preserved,
            u64::from(preserved_block) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(preserved, [0x9d; BLOCK_SIZE]);
        Ok(())
    }

    #[test]
    fn rejects_invalid_footer_above_maximum_payloads() -> io::Result<()> {
        let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
        let clear_start = layout.log_start + 2;
        let invalid_footer_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
        let backing_blocks = u64::from(invalid_footer_block) + 1;
        let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        file.write_all_at(
            &vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE],
            u64::from(clear_start) * BLOCK_SIZE as u64,
        )?;
        let mut footer = raw_footer(volume_id, 2, layout.log_start + 1, invalid_footer_block);
        assert_raw_footer_checksum_valid(&footer);
        let replay_cursor = Checkpoint {
            slot: CheckpointSlot::Green,
            volume_id,
            volume_blocks: 4,
            backing_blocks,
            checkpoint_lsn: 1,
            last_footer_block: layout.log_start + 1,
            layout,
            body_checksum: 0,
        };
        assert!(
            decode_write_tail_footer(&mut footer, replay_cursor, invalid_footer_block).is_none()
        );
        file.write_all_at(&footer, u64::from(invalid_footer_block) * BLOCK_SIZE as u64)?;
        file.sync_all()?;
        drop(file);

        verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
        let file = OpenOptions::new().read(true).open(&backing.0)?;
        assert_zero_blocks(&file, clear_start, MAXIMUM_RECORD_BLOCKS as u32)?;
        let mut preserved_footer = [0; BLOCK_SIZE];
        file.read_exact_at(
            &mut preserved_footer,
            u64::from(invalid_footer_block) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(preserved_footer, footer);
        Ok(())
    }

    #[test]
    fn rejects_invalid_footer_declaring_position_beyond_backing() -> io::Result<()> {
        const BACKING_REMAINDER_BLOCKS: u32 = 5;

        let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
        let clear_start = layout.log_start + 2;
        let backing_blocks = u64::from(clear_start + BACKING_REMAINDER_BLOCKS);
        let (backing, layout, volume_id) = prepare_footer_rejection_image(backing_blocks)?;
        let actual_footer_block = backing_blocks as u32 - 1;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        file.write_all_at(
            &vec![0x7e; BACKING_REMAINDER_BLOCKS as usize * BLOCK_SIZE],
            u64::from(clear_start) * BLOCK_SIZE as u64,
        )?;
        let mut footer = raw_footer(volume_id, 2, layout.log_start + 1, backing_blocks as u32);
        for index in 0..(BACKING_REMAINDER_BLOCKS - 1) as usize {
            let lba_offset = FOOTER_LBA_OFFSET + index * size_of::<u32>();
            let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + index * size_of::<u64>();
            footer[lba_offset..lba_offset + size_of::<u32>()]
                .copy_from_slice(&(index as u32).to_le_bytes());
            let payload_block = clear_start + index as u32;
            let checksum =
                payload_checksum(volume_id, index as u32, payload_block, &[0x7e; BLOCK_SIZE]);
            footer[checksum_offset..checksum_offset + size_of::<u64>()]
                .copy_from_slice(&checksum.to_le_bytes());
        }
        seal_raw_footer(&mut footer);
        assert_raw_footer_checksum_valid(&footer);
        file.write_all_at(&footer, u64::from(actual_footer_block) * BLOCK_SIZE as u64)?;
        file.sync_all()?;
        drop(file);

        verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
        let file = OpenOptions::new().read(true).open(&backing.0)?;
        assert_eq!(file.metadata()?.len(), backing_blocks * BLOCK_SIZE as u64);
        assert_zero_blocks(&file, clear_start, BACKING_REMAINDER_BLOCKS)
    }

    #[test]
    fn rejects_invalid_footer_with_out_of_range_lba() -> io::Result<()> {
        verify_rejected_two_payload_footer(|footer, volume_id, first_payload_block| {
            footer[FOOTER_LBA_OFFSET..FOOTER_LBA_OFFSET + size_of::<u32>()]
                .copy_from_slice(&4_u32.to_le_bytes());
            let checksum = payload_checksum(volume_id, 4, first_payload_block, &[0x5a; BLOCK_SIZE]);
            footer
                [FOOTER_PAYLOAD_CHECKSUM_OFFSET..FOOTER_PAYLOAD_CHECKSUM_OFFSET + size_of::<u64>()]
                .copy_from_slice(&checksum.to_le_bytes());
        })
    }

    #[test]
    fn rejects_invalid_footer_with_duplicate_lbas() -> io::Result<()> {
        verify_rejected_two_payload_footer(|footer, volume_id, first_payload_block| {
            let first_lba =
                footer[FOOTER_LBA_OFFSET..FOOTER_LBA_OFFSET + size_of::<u32>()].to_vec();
            footer[FOOTER_LBA_OFFSET + size_of::<u32>()..FOOTER_LBA_OFFSET + 2 * size_of::<u32>()]
                .copy_from_slice(&first_lba);
            let checksum =
                payload_checksum(volume_id, 1, first_payload_block + 1, &[0xc3; BLOCK_SIZE]);
            let checksum_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + size_of::<u64>();
            footer[checksum_offset..checksum_offset + size_of::<u64>()]
                .copy_from_slice(&checksum.to_le_bytes());
        })
    }

    #[test]
    fn rejects_invalid_footer_with_nonzero_unused_lba() -> io::Result<()> {
        verify_rejected_two_payload_footer(|footer, _, _| {
            let unused_offset = FOOTER_LBA_OFFSET + 2 * size_of::<u32>();
            footer[unused_offset..unused_offset + size_of::<u32>()]
                .copy_from_slice(&3_u32.to_le_bytes());
        })
    }

    #[test]
    fn rejects_invalid_footer_with_nonzero_unused_checksum() -> io::Result<()> {
        verify_rejected_two_payload_footer(|footer, _, _| {
            let unused_offset = FOOTER_PAYLOAD_CHECKSUM_OFFSET + 2 * size_of::<u64>();
            footer[unused_offset..unused_offset + size_of::<u64>()]
                .copy_from_slice(&1_u64.to_le_bytes());
        })
    }

    fn recover_after_stale_tail_crash(test_name: &str, failpoint: &str) -> io::Result<()> {
        if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
            let _volume = open(PathBuf::from(backing))?;
            return Err(io::Error::other("test failpoint did not pause child"));
        }

        let fixture = prepare_invalid_gap_fixture()?;
        sigkill_child_at_handshake(test_name, &fixture.backing.0, failpoint, Some(failpoint))?;
        if failpoint == STALE_TAIL_FSYNC_COMPLETE_FAILPOINT {
            verify_cleared_window(&fixture)?;
        }
        verify_invalid_gap_recovery(&fixture)
    }

    #[test]
    fn recover_after_each_stale_tail_clear_block_boundary() -> io::Result<()> {
        let plan = configured_crash_test_plan(MAXIMUM_RECORD_BLOCKS)?;
        for block_index in plan.boundary_indices {
            let failpoint = format!("{STALE_TAIL_CLEAR_BLOCK_COMPLETE_FAILPOINT}-{block_index}");
            for _ in 0..plan.repetitions {
                recover_after_stale_tail_crash(
                    "tests::recover_after_each_stale_tail_clear_block_boundary",
                    &failpoint,
                )?;
            }
        }
        Ok(())
    }

    #[test]
    fn recover_after_stale_tail_fsync_boundary() -> io::Result<()> {
        for _ in 0..configured_crash_test_plan(1)?.repetitions {
            recover_after_stale_tail_crash(
                "tests::recover_after_stale_tail_fsync_boundary",
                STALE_TAIL_FSYNC_COMPLETE_FAILPOINT,
            )?;
        }
        Ok(())
    }

    #[test]
    fn invalid_gap_stops_replay_and_clears_only_bounded_window() -> io::Result<()> {
        let fixture = prepare_invalid_gap_fixture()?;
        verify_invalid_gap_recovery(&fixture)
    }

    #[test]
    fn stale_tail_clear_stops_at_backing_eof() -> io::Result<()> {
        const EOF_REMAINDER_BLOCKS: u32 = 17;

        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        let clear_start = layout.log_start + 2;
        let backing_bytes = u64::from(clear_start + EOF_REMAINDER_BLOCKS) * BLOCK_SIZE as u64;
        backing.create_sized(backing_bytes)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let volume_id = formatted_volume_id(&backing)?;

        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        write_raw_record(
            &file,
            volume_id,
            1,
            layout.log_start - 1,
            layout.log_start,
            0,
            0xa5,
        )?;
        file.write_all_at(
            &vec![0x7e; EOF_REMAINDER_BLOCKS as usize * BLOCK_SIZE],
            u64::from(clear_start) * BLOCK_SIZE as u64,
        )?;
        file.sync_all()?;
        drop(file);

        let volume = open(&backing.0)?;
        assert_eq!(volume.last_lsn, 1);
        drop(volume);
        let file = OpenOptions::new().read(true).open(&backing.0)?;
        assert_eq!(file.metadata()?.len(), backing_bytes);
        for block in clear_start..clear_start + EOF_REMAINDER_BLOCKS {
            let mut bytes = [0xa5; BLOCK_SIZE];
            file.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
            assert_eq!(bytes, [0; BLOCK_SIZE]);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_checkpoint_after_bytes_before_touching_backing() -> io::Result<()> {
        const MINIMUM_CHECKPOINT_AFTER_BYTES: u64 = 339 * 4096;

        assert_eq!(
            MINIMUM_CHECKPOINT_AFTER_BYTES,
            MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64
        );
        assert_eq!(
            VolumeOpenOptions::default().checkpoint_after_bytes,
            64 * 1024 * 1024
        );

        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(
            u64::from(layout.log_start + MAXIMUM_RECORD_BLOCKS as u32) * BLOCK_SIZE as u64,
        )?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let file = OpenOptions::new().write(true).open(&backing.0)?;
        file.write_all_at(
            &[0xa5; BLOCK_SIZE],
            u64::from(layout.log_start) * BLOCK_SIZE as u64,
        )?;
        file.sync_all()?;
        drop(file);
        let original_bytes = std::fs::read(&backing.0)?;

        let missing = TemporaryBacking::new()?;
        let invalid_values = [
            0,
            MINIMUM_CHECKPOINT_AFTER_BYTES - BLOCK_SIZE as u64,
            MINIMUM_CHECKPOINT_AFTER_BYTES - BLOCK_SIZE as u64 + 1,
            MINIMUM_CHECKPOINT_AFTER_BYTES - 1,
            MINIMUM_CHECKPOINT_AFTER_BYTES + 1,
            VolumeOpenOptions::default().checkpoint_after_bytes - 1,
            VolumeOpenOptions::default().checkpoint_after_bytes + 1,
        ];
        for checkpoint_after_bytes in invalid_values {
            let options = VolumeOpenOptions {
                checkpoint_after_bytes,
            };
            for path in [&backing.0, &missing.0] {
                let error = match open_with_options(path, options) {
                    Ok(_) => return Err(io::Error::other("invalid replay bound was accepted")),
                    Err(error) => error,
                };
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            }
            assert_eq!(std::fs::read(&backing.0)?, original_bytes);
            assert!(!missing.0.exists());
        }

        let minimum = VolumeOpenOptions {
            checkpoint_after_bytes: MINIMUM_CHECKPOINT_AFTER_BYTES,
        };
        let volume = open_with_options(&backing.0, minimum)?;
        assert_eq!(
            volume.checkpoint_after_bytes,
            MINIMUM_CHECKPOINT_AFTER_BYTES
        );
        drop(volume);

        let defaults = VolumeOpenOptions::default();
        let volume = open_with_options(&backing.0, defaults)?;
        assert_eq!(volume.checkpoint_after_bytes, 64 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn checkpoints_before_write_exceeding_replay_bound() -> io::Result<()> {
        const CHECKPOINT_BLOCKS: u64 = 340;
        const FIRST_BOUND_LSN: u64 = CHECKPOINT_BLOCKS / 2;
        const SECOND_BOUND_LSN: u64 = FIRST_BOUND_LSN * 2;

        assert_eq!(
            VolumeOpenOptions::default().checkpoint_after_bytes,
            64 * 1024 * 1024
        );
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 800) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let options = VolumeOpenOptions {
            checkpoint_after_bytes: CHECKPOINT_BLOCKS * BLOCK_SIZE as u64,
        };
        let mut volume = open_with_options(&backing.0, options)?;

        for lsn in 1..=FIRST_BOUND_LSN {
            let lba = (lsn % u64::from(volume_blocks)) as u32;
            volume.write_block(lba, &[lsn as u8; BLOCK_SIZE])?;
        }
        assert_eq!(volume.checkpoint_lsn, 0);
        assert_eq!(volume.last_lsn, FIRST_BOUND_LSN);
        assert_eq!(
            volume.log_bytes_since_checkpoint,
            options.checkpoint_after_bytes
        );
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);

        let file = File::open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        assert_eq!(read_u64(&descriptors[BLOCK_SIZE..], 32), 0);
        let exact_footer = volume.last_footer_block;
        let exact_physical_blocks = volume.physical_blocks.clone();
        let exact_checksums = volume.checksums.clone();

        assert_eq!(
            volume
                .write_block(volume_blocks, &[0xff; BLOCK_SIZE])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let mut unchanged_descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut unchanged_descriptors, 0)?;
        assert_eq!(unchanged_descriptors, descriptors);
        assert_eq!(volume.checkpoint_lsn, 0);
        assert_eq!(
            volume.log_bytes_since_checkpoint,
            options.checkpoint_after_bytes
        );

        volume.write_block(1, &[(FIRST_BOUND_LSN + 1) as u8; BLOCK_SIZE])?;
        assert_eq!(volume.last_lsn, FIRST_BOUND_LSN + 1);
        assert_eq!(volume.durable_lsn, FIRST_BOUND_LSN);
        assert_eq!(volume.checkpoint_lsn, FIRST_BOUND_LSN);
        assert_eq!(volume.last_footer_block, exact_footer + 2);
        assert_eq!(volume.log_bytes_since_checkpoint, (2 * BLOCK_SIZE) as u64);
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Green);
        assert_ne!(volume.physical_blocks[1], exact_physical_blocks[1]);

        let mut blue_descriptor = [0; BLOCK_SIZE];
        file.read_exact_at(&mut blue_descriptor, BLOCK_SIZE as u64)?;
        assert_eq!(read_u64(&blue_descriptor, 32), FIRST_BOUND_LSN);
        assert_eq!(read_u32(&blue_descriptor, 40), exact_footer);
        let mut blue_body = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut blue_body,
            u64::from(layout.blue_physical_map_start) * BLOCK_SIZE as u64,
        )?;
        for lba in 0..volume_blocks as usize {
            assert_eq!(read_u32(&blue_body, lba * 4), exact_physical_blocks[lba]);
            assert_eq!(
                read_u64(&blue_body, BLOCK_SIZE + lba * 8),
                exact_checksums[lba]
            );
        }
        assert_eq!(
            read_u64(&blue_descriptor, CHECKPOINT_BODY_CHECKSUM_OFFSET),
            xxh3_64(&blue_body)
        );

        for lsn in FIRST_BOUND_LSN + 2..=SECOND_BOUND_LSN {
            let lba = (lsn % u64::from(volume_blocks)) as u32;
            volume.write_block(lba, &[lsn as u8; BLOCK_SIZE])?;
        }
        assert_eq!(volume.checkpoint_lsn, FIRST_BOUND_LSN);
        assert_eq!(
            volume.log_bytes_since_checkpoint,
            options.checkpoint_after_bytes
        );
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Green);
        let second_footer = volume.last_footer_block;
        let second_physical_blocks = volume.physical_blocks.clone();
        let second_checksums = volume.checksums.clone();

        volume.write_block(1, &[(SECOND_BOUND_LSN + 1) as u8; BLOCK_SIZE])?;
        assert_eq!(volume.last_lsn, SECOND_BOUND_LSN + 1);
        assert_eq!(volume.durable_lsn, SECOND_BOUND_LSN);
        assert_eq!(volume.checkpoint_lsn, SECOND_BOUND_LSN);
        assert_eq!(volume.last_footer_block, second_footer + 2);
        assert_eq!(volume.log_bytes_since_checkpoint, (2 * BLOCK_SIZE) as u64);
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);

        let mut green_descriptor = [0; BLOCK_SIZE];
        file.read_exact_at(&mut green_descriptor, 0)?;
        assert_eq!(read_u64(&green_descriptor, 32), SECOND_BOUND_LSN);
        assert_eq!(read_u32(&green_descriptor, 40), second_footer);
        let mut green_body = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut green_body,
            u64::from(layout.green_physical_map_start) * BLOCK_SIZE as u64,
        )?;
        for lba in 0..volume_blocks as usize {
            assert_eq!(read_u32(&green_body, lba * 4), second_physical_blocks[lba]);
            assert_eq!(
                read_u64(&green_body, BLOCK_SIZE + lba * 8),
                second_checksums[lba]
            );
        }
        assert_eq!(
            read_u64(&green_descriptor, CHECKPOINT_BODY_CHECKSUM_OFFSET),
            xxh3_64(&green_body)
        );

        drop(volume);
        let mut reopened = open_with_options(&backing.0, options)?;
        assert_eq!(reopened.last_lsn, SECOND_BOUND_LSN + 1);
        assert_eq!(reopened.durable_lsn, SECOND_BOUND_LSN + 1);
        assert_eq!(reopened.checkpoint_lsn, SECOND_BOUND_LSN);
        assert_eq!(reopened.last_footer_block, second_footer + 2);
        assert_eq!(reopened.log_bytes_since_checkpoint, (2 * BLOCK_SIZE) as u64);
        assert_eq!(reopened.next_checkpoint_slot, CheckpointSlot::Blue);
        let mut actual = [0; BLOCK_SIZE];
        reopened.read_block(0, &mut actual)?;
        assert_eq!(actual, [SECOND_BOUND_LSN as u8; BLOCK_SIZE]);
        reopened.read_block(1, &mut actual)?;
        assert_eq!(actual, [(SECOND_BOUND_LSN + 1) as u8; BLOCK_SIZE]);
        Ok(())
    }

    #[test]
    fn log_full_write_does_not_publish_live_checkpoint() -> io::Result<()> {
        const CHECKPOINT_BLOCKS: u64 = 340;

        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(
            u64::from(layout.log_start) * BLOCK_SIZE as u64 + CHECKPOINT_BLOCKS * BLOCK_SIZE as u64,
        )?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let options = VolumeOpenOptions {
            checkpoint_after_bytes: CHECKPOINT_BLOCKS * BLOCK_SIZE as u64,
        };
        let mut volume = open_with_options(&backing.0, options)?;
        for lsn in 1..=CHECKPOINT_BLOCKS / 2 {
            volume.write_block(0, &[lsn as u8; BLOCK_SIZE])?;
        }

        let state = (
            volume.physical_blocks.clone(),
            volume.checksums.clone(),
            volume.last_lsn,
            volume.durable_lsn,
            volume.checkpoint_lsn,
            volume.last_footer_block,
            volume.next_checkpoint_slot,
            volume.log_bytes_since_checkpoint,
        );
        let file = File::open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        assert_eq!(
            volume
                .write_block(0, &[0xff; BLOCK_SIZE])
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert_eq!(
            (
                volume.physical_blocks.clone(),
                volume.checksums.clone(),
                volume.last_lsn,
                volume.durable_lsn,
                volume.checkpoint_lsn,
                volume.last_footer_block,
                volume.next_checkpoint_slot,
                volume.log_bytes_since_checkpoint,
            ),
            state
        );
        let mut unchanged_descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut unchanged_descriptors, 0)?;
        assert_eq!(unchanged_descriptors, descriptors);
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [(CHECKPOINT_BLOCKS / 2) as u8; BLOCK_SIZE]);
        Ok(())
    }

    #[test]
    fn flush_advances_durable_lsn() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;

        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        assert_eq!(volume.durable_lsn, 0);
        volume.flush()?;
        assert_eq!(volume.durable_lsn, volume.last_lsn);
        Ok(())
    }

    #[test]
    fn close_persists_clean_checkpoint_body_and_descriptor() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 2_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;
        let volume_id = volume.volume_id;
        let payload = [0xa5; BLOCK_SIZE];
        let payload_block = volume.last_footer_block + 1;
        let footer_block = payload_block + 1;
        let lba = 1_u32;
        volume.write_block(lba, &payload)?;
        volume.close()?;

        let file = File::open(&backing.0)?;
        let mut body = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut body,
            u64::from(layout.blue_physical_map_start) * BLOCK_SIZE as u64,
        )?;
        assert_eq!(read_u32(&body, 0), 0);
        assert_eq!(read_u32(&body, 4), payload_block);
        assert!(body[8..BLOCK_SIZE].iter().all(|byte| *byte == 0));

        let mut checksum_input = [0; BLOCK_SIZE + 8];
        checksum_input[..4].copy_from_slice(&lba.to_le_bytes());
        checksum_input[4..8].copy_from_slice(&payload_block.to_le_bytes());
        checksum_input[8..].copy_from_slice(&payload);
        let payload_checksum = xxh3_64_with_seed(&checksum_input, volume_id);
        assert_eq!(read_u64(&body, BLOCK_SIZE), 0);
        assert_eq!(read_u64(&body, BLOCK_SIZE + 8), payload_checksum);
        assert!(body[BLOCK_SIZE + 16..].iter().all(|byte| *byte == 0));

        let mut descriptor = [0; BLOCK_SIZE];
        file.read_exact_at(&mut descriptor, BLOCK_SIZE as u64)?;
        assert_eq!(&descriptor[..4], CHECKPOINT_MAGIC);
        assert_eq!(&descriptor[4..8], &[FORMAT_VERSION, 1, 0, 0]);
        assert_eq!(read_u64(&descriptor, 8), volume_id);
        assert_eq!(read_u64(&descriptor, 32), 1);
        assert_eq!(read_u32(&descriptor, 40), footer_block);
        assert_eq!(
            read_u64(&descriptor, CHECKPOINT_BODY_CHECKSUM_OFFSET),
            xxh3_64(&body)
        );
        assert!(
            descriptor[CHECKPOINT_BODY_CHECKSUM_OFFSET + 8..DESCRIPTOR_CHECKSUM_OFFSET]
                .iter()
                .all(|byte| *byte == 0)
        );
        let stored_descriptor_checksum = read_u64(&descriptor, DESCRIPTOR_CHECKSUM_OFFSET);
        descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        assert_eq!(xxh3_64(&descriptor), stored_descriptor_checksum);

        let reopened = open(&backing.0)?;
        assert_eq!(reopened.physical_blocks, [0, payload_block]);
        assert_eq!(reopened.checksums, [0, payload_checksum]);
        assert_eq!(reopened.last_lsn, 1);
        assert_eq!(reopened.durable_lsn, 1);
        assert_eq!(reopened.checkpoint_lsn, 1);
        assert_eq!(reopened.last_footer_block, footer_block);
        assert_eq!(reopened.next_checkpoint_slot, CheckpointSlot::Green);
        Ok(())
    }

    #[test]
    fn close_without_mapping_changes_does_not_publish_checkpoint() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;

        let file = File::open(&backing.0)?;
        let mut before = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut before, 0)?;
        open(&backing.0)?.close()?;
        let mut after = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut after, 0)?;
        assert_eq!(after, before);

        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.close()?;
        file.read_exact_at(&mut before, 0)?;
        open(&backing.0)?.close()?;
        file.read_exact_at(&mut after, 0)?;
        assert_eq!(after, before);
        Ok(())
    }

    #[test]
    fn equal_lsn_checkpoints_require_equivalent_mapping() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;
        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.close()?;

        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let mut blue = [0; BLOCK_SIZE];
        file.read_exact_at(&mut blue, BLOCK_SIZE as u64)?;
        let mut body = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(
            &mut body,
            u64::from(layout.blue_physical_map_start) * BLOCK_SIZE as u64,
        )?;
        file.write_all_at(
            &body,
            u64::from(layout.green_physical_map_start) * BLOCK_SIZE as u64,
        )?;
        let mut green = blue;
        green[5] = CheckpointSlot::Green as u8;
        green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        let checksum = xxh3_64(&green);
        green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        file.write_all_at(&green, 0)?;

        let volume = open(&backing.0)?;
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);
        drop(volume);

        green[CHECKPOINT_BODY_CHECKSUM_OFFSET] ^= 1;
        green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        let checksum = xxh3_64(&green);
        green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        file.write_all_at(&green, 0)?;
        assert_eq!(
            open(&backing.0).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }

    #[test]
    fn format_validates_geometry_boundaries() -> io::Result<()> {
        assert!(layout_for(0).is_err());
        assert!(layout_for(BLOCK_SIZE as u64 - 1).is_err());
        assert!(layout_for((MAX_VOLUME_BLOCKS + 1) * BLOCK_SIZE as u64).is_err());

        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        assert_eq!(
            backing_blocks_for(&layout, MAX_BACKING_BLOCKS * BLOCK_SIZE as u64)?,
            MAX_BACKING_BLOCKS
        );
        assert!(backing_blocks_for(&layout, (MAX_BACKING_BLOCKS + 1) * BLOCK_SIZE as u64).is_err());
        assert!(backing_blocks_for(&layout, u64::from(layout.log_start + 2) * 4096 - 1).is_err());
        assert!(backing_blocks_for(&layout, u64::from(layout.log_start + 1) * 4096).is_err());

        let missing = TemporaryBacking::new()?;
        assert_eq!(
            format(&missing.0, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let too_small = TemporaryBacking::new()?;
        too_small.create_sized(u64::from(layout.log_start + 1) * BLOCK_SIZE as u64)?;
        assert_eq!(
            format(&too_small.0, BLOCK_SIZE as u64).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let misaligned = TemporaryBacking::new()?;
        misaligned.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64 - 1)?;
        assert_eq!(
            format(&misaligned.0, BLOCK_SIZE as u64).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        Ok(())
    }

    #[test]
    fn open_reconstructs_empty_volume_and_requires_matching_roots() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 1025_u32;
        let backing_blocks = 14_u64;
        backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        let volume_id = read_u64(&descriptors, 8);
        let blue_descriptor = descriptors[BLOCK_SIZE..].to_vec();

        let volume = open(&backing.0)?;
        assert_eq!(volume.volume_id, volume_id);
        assert_eq!(volume.volume_blocks, volume_blocks);
        assert_eq!(volume.backing_blocks, backing_blocks);
        assert!(volume.physical_blocks.iter().all(|block| *block == 0));
        assert!(volume.checksums.iter().all(|checksum| *checksum == 0));
        assert_eq!(volume.last_lsn, 0);
        assert_eq!(volume.durable_lsn, 0);
        assert_eq!(volume.checkpoint_lsn, 0);
        assert_eq!(volume.last_footer_block, 11);
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);
        assert_eq!(volume.log_bytes_since_checkpoint, 0);
        drop(volume);

        let green = &mut descriptors[..BLOCK_SIZE];
        green[0] ^= 1;
        file.write_all_at(green, 0)?;
        let volume = open(&backing.0)?;
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Green);
        drop(volume);

        green.copy_from_slice(&blue_descriptor);
        green[5] = CheckpointSlot::Green as u8;
        green[8] ^= 1;
        green[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
        let checksum = xxh3_64(green);
        green[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        file.write_all_at(green, 0)?;
        assert_eq!(
            open(&backing.0).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );

        Ok(())
    }

    #[test]
    fn format_writes_valid_empty_checkpoint_roots() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 1025_u32;
        let backing_blocks = 14_u64;
        backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        let checkpoint_bodies = vec![0xa5; (layout.log_start as usize - 2) * BLOCK_SIZE];
        OpenOptions::new()
            .write(true)
            .open(&backing.0)?
            .write_all_at(&checkpoint_bodies, 2 * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let file = File::open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        let volume_id = read_u64(&descriptors, 8);

        for index in 0..2 {
            let descriptor = &descriptors[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE];
            assert_eq!(&descriptor[..4], CHECKPOINT_MAGIC);
            assert_eq!(descriptor[4], FORMAT_VERSION);
            assert_eq!(descriptor[5], index as u8);
            assert_eq!(&descriptor[6..8], &[0, 0]);
            assert_eq!(read_u64(descriptor, 8), volume_id);
            assert_eq!(read_u32(descriptor, 16), BLOCK_SIZE as u32);
            assert_eq!(read_u32(descriptor, 20), volume_blocks);
            assert_eq!(read_u64(descriptor, 24), backing_blocks);
            assert_eq!(read_u64(descriptor, 32), 0);
            assert_eq!(read_u32(descriptor, 40), 11);
            assert_eq!(read_u64(descriptor, CHECKPOINT_BODY_CHECKSUM_OFFSET), 0);
            assert!(
                descriptor[CHECKPOINT_BODY_CHECKSUM_OFFSET + 8..DESCRIPTOR_CHECKSUM_OFFSET]
                    .iter()
                    .all(|byte| *byte == 0)
            );

            let expected_checksum = read_u64(descriptor, DESCRIPTOR_CHECKSUM_OFFSET);
            let mut checksummed = descriptor.to_vec();
            checksummed[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
            assert_eq!(xxh3_64(&checksummed), expected_checksum);
        }

        let mut preserved_bodies = vec![0; checkpoint_bodies.len()];
        file.read_exact_at(&mut preserved_bodies, 2 * BLOCK_SIZE as u64)?;
        assert_eq!(preserved_bodies, checkpoint_bodies);
        let volume = open(&backing.0)?;
        assert_eq!(volume.next_checkpoint_slot, CheckpointSlot::Blue);
        drop(volume);

        let mut empty_log = [1; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut empty_log, 12 * BLOCK_SIZE as u64)?;
        assert!(empty_log.iter().all(|byte| *byte == 0));

        for descriptor in descriptors.chunks_exact_mut(BLOCK_SIZE) {
            descriptor[6] = 1;
            descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
            let checksum = xxh3_64(descriptor);
            descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        }
        OpenOptions::new()
            .write(true)
            .open(&backing.0)?
            .write_all_at(&descriptors, 0)?;
        assert_eq!(
            open(&backing.0).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
}
