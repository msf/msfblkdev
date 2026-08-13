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
const FOOTER_LBA_OFFSET: usize = 32;
const FOOTER_PAYLOAD_CHECKSUM_OFFSET: usize = 1384;
const FOOTER_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
const FOOTER_PAYLOAD_MAX: u32 =
    ((FOOTER_PAYLOAD_CHECKSUM_OFFSET - FOOTER_LBA_OFFSET) / size_of::<u32>()) as u32;
const CHECKPOINT_BODY_CHECKSUM_OFFSET: usize = 44;
const DESCRIPTOR_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
const MAX_VOLUME_BLOCKS: u64 = 1 << 31;
const MAX_BACKING_BLOCKS: u64 = u32::MAX as u64 + 1;

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
    failed: bool,
}

impl Volume {
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
        self.durable_lsn = self.last_lsn;
        Ok(())
    }

    /// Flushes and checkpoints changed mapping state before releasing its resources.
    pub fn close(mut self) -> io::Result<()> {
        self.flush()?;
        if self.last_lsn == self.checkpoint_lsn {
            return Ok(());
        }

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
        self.flush()?;
        self.checkpoint_lsn = self.last_lsn;
        self.log_bytes_since_checkpoint = 0;
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
            self.write_checkpoint_body_block(
                body_start + layout.physical_map_blocks + block_index,
                &block,
            )?;
        }
        self.flush()?;
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
            return Err(io::Error::other("log full"));
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

        self.physical_blocks[lba as usize] = payload_block;
        self.checksums[lba as usize] = checksum;
        self.last_lsn = lsn;
        self.last_footer_block = footer_block;
        self.log_bytes_since_checkpoint += record.0.len() as u64;
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
    footer[5] = 1;
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

fn is_expected_tail_footer(
    footer: &mut [u8],
    checkpoint: Checkpoint,
    footer_block: u32,
    payload_count: u32,
) -> bool {
    let stored_checksum = read_u64(footer, FOOTER_CHECKSUM_OFFSET);
    footer[FOOTER_CHECKSUM_OFFSET..].fill(0);
    let checksum_valid = xxh3_64(footer) == stored_checksum;
    footer[FOOTER_CHECKSUM_OFFSET..].copy_from_slice(&stored_checksum.to_le_bytes());
    if !checksum_valid
        || &footer[..4] != FOOTER_MAGIC
        || footer[4] != FORMAT_VERSION
        || footer[5] != 1
        || u16::from_le_bytes(footer[6..8].try_into().unwrap()) != 0
        || read_u64(footer, 8) != checkpoint.volume_id
        || checkpoint.checkpoint_lsn == u64::MAX
        || read_u64(footer, 16) != checkpoint.checkpoint_lsn + 1
        || read_u32(footer, 24) != checkpoint.last_footer_block
        || read_u32(footer, 28) != footer_block
    {
        return false;
    }
    (0..payload_count).all(|index| {
        read_u32(
            footer,
            FOOTER_LBA_OFFSET + index as usize * size_of::<u32>(),
        ) < checkpoint.volume_blocks
    })
}

fn has_unreplayed_tail(
    ring: &mut Option<IoUring>,
    backing: &File,
    checkpoint: Checkpoint,
) -> io::Result<bool> {
    let mut footer = AlignedBlock([0; BLOCK_SIZE]);
    for payload_count in 1..=FOOTER_PAYLOAD_MAX {
        let footer_block = u64::from(checkpoint.last_footer_block) + u64::from(payload_count) + 1;
        if footer_block >= checkpoint.backing_blocks {
            break;
        }
        read_checkpoint_body_block(ring, backing, footer_block as u32, &mut footer)?;
        if is_expected_tail_footer(
            &mut footer.0,
            checkpoint,
            footer_block as u32,
            payload_count,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
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

/// Opens a formatted volume from its two empty checkpoint roots.
pub fn open(backing_path: impl AsRef<Path>) -> io::Result<Volume> {
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
    let (checkpoint, physical_blocks, checksums) =
        recovered.ok_or_else(|| invalid_data("no valid checkpoint body"))?;
    if has_unreplayed_tail(&mut ring, &backing, checkpoint)? {
        return Err(invalid_data("checkpoint tail replay required"));
    }

    Ok(Volume {
        backing,
        ring,
        volume_id: checkpoint.volume_id,
        volume_blocks: checkpoint.volume_blocks,
        backing_blocks: checkpoint.backing_blocks,
        physical_blocks,
        checksums,
        last_lsn: checkpoint.checkpoint_lsn,
        durable_lsn: checkpoint.checkpoint_lsn,
        checkpoint_lsn: checkpoint.checkpoint_lsn,
        last_footer_block: checkpoint.last_footer_block,
        next_checkpoint_slot: match checkpoint.slot {
            CheckpointSlot::Green => CheckpointSlot::Blue,
            CheckpointSlot::Blue => CheckpointSlot::Green,
        },
        log_bytes_since_checkpoint: 0,
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
    use std::fs::remove_file;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::FileExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
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

    #[test]
    fn xxh3_matches_zig_vectors() {
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
        assert_eq!(&footer[4..8], &[FORMAT_VERSION, 1, 0, 0]);
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
    fn failed_write_is_not_published_and_close_releases_resources() -> io::Result<()> {
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

        let backing_fd = volume.backing.as_raw_fd();
        let ring_fd = volume.ring.as_ref().unwrap().as_raw_fd();
        assert_eq!(volume.close().unwrap_err().to_string(), "volume failed");
        assert_eq!(unsafe { libc::fcntl(backing_fd, libc::F_GETFD) }, -1);
        assert_eq!(unsafe { libc::fcntl(ring_fd, libc::F_GETFD) }, -1);
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
    fn open_rejects_durable_uncheckpointed_tail() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;

        let mut volume = open(&backing.0)?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.flush()?;
        drop(volume);

        assert_eq!(
            open(&backing.0).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
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
