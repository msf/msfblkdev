#[cfg(any(test, feature = "test-failpoints"))]
use super::pause_at_test_failpoint;
use super::{
    AlignedBlock, BLOCK_SIZE, FORMAT_VERSION, Layout, Volume, backing_blocks_for, layout_for,
    read_block_exact, read_u32, read_u64, submit_exact,
};
use io_uring::{IoUring, opcode, types};
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

pub(super) const CHECKPOINT_MAGIC: &[u8; 4] = b"VBLC";
pub(super) const CHECKPOINT_BODY_CHECKSUM_OFFSET: usize = 44;
pub(super) const DESCRIPTOR_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
pub(super) const DEFAULT_CHECKPOINT_AFTER_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(any(test, feature = "test-failpoints"))]
pub(super) const CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT: &str = "checkpoint-body-block-complete";
#[cfg(any(test, feature = "test-failpoints"))]
pub(super) const CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT: &str = "checkpoint-body-fsync-complete";
#[cfg(any(test, feature = "test-failpoints"))]
pub(super) const DESCRIPTOR_WRITE_COMPLETE_FAILPOINT: &str = "descriptor-write-complete";
#[cfg(any(test, feature = "test-failpoints"))]
pub(super) const DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT: &str = "descriptor-fsync-complete";

#[repr(align(4096))]
pub(super) struct AlignedDescriptors(pub(super) [u8; 2 * BLOCK_SIZE]);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum CheckpointSlot {
    Green = 0,
    Blue = 1,
}

#[derive(Clone, Copy)]
pub(super) struct Checkpoint {
    pub(super) slot: CheckpointSlot,
    pub(super) volume_id: u64,
    pub(super) volume_blocks: u32,
    pub(super) backing_blocks: u64,
    pub(super) checkpoint_lsn: u64,
    pub(super) last_footer_block: u32,
    pub(super) layout: Layout,
    pub(super) body_checksum: u64,
}

impl Volume {
    pub(super) fn publish_checkpoint(&mut self) -> io::Result<()> {
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
        #[cfg(any(test, feature = "test-failpoints"))]
        pause_at_test_failpoint(DESCRIPTOR_WRITE_COMPLETE_FAILPOINT)?;
        self.flush()?;
        #[cfg(any(test, feature = "test-failpoints"))]
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
            #[cfg(any(test, feature = "test-failpoints"))]
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
            #[cfg(any(test, feature = "test-failpoints"))]
            pause_at_test_failpoint(&format!(
                "{CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT}-{body_block_index}"
            ))?;
        }
        self.flush()?;
        #[cfg(any(test, feature = "test-failpoints"))]
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
}

pub(super) fn encode_checkpoint_descriptor(descriptor: &mut [u8], checkpoint: &Checkpoint) {
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

pub(super) fn decode_checkpoint(
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

pub(super) fn load_checkpoint_body(
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
        read_block_exact(ring, backing, body_start + block_index, &mut block)?;
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
        read_block_exact(
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
