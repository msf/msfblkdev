#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

const BLOCK_SIZE: usize = 4096;
const DESCRIPTOR_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u64>();
const MAX_VOLUME_BLOCKS: u64 = 1 << 31;
const MAX_BACKING_BLOCKS: u64 = u32::MAX as u64 + 1;

#[cfg(test)]
#[repr(align(4096))]
struct AlignedBlock([u8; BLOCK_SIZE]);

#[repr(align(4096))]
struct AlignedDescriptors([u8; 2 * BLOCK_SIZE]);

#[derive(Clone, Copy)]
enum CheckpointSlot {
    Green = 0,
    Blue = 1,
}

#[derive(Debug, PartialEq)]
struct Layout {
    physical_map_blocks: u32,
    checksum_map_blocks: u32,
    green_physical_map_start: u32,
    green_checksum_map_start: u32,
    blue_physical_map_start: u32,
    blue_checksum_map_start: u32,
    log_start: u32,
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
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

fn encode_checkpoint_descriptor(
    descriptor: &mut [u8],
    slot: CheckpointSlot,
    generation: u64,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    layout: &Layout,
) {
    descriptor.fill(0);
    descriptor[..4].copy_from_slice(b"VBLC");
    descriptor[4] = 1;
    descriptor[5] = slot as u8;
    descriptor[6..8].copy_from_slice(&1_u16.to_le_bytes());
    descriptor[8..16].copy_from_slice(&volume_id.to_le_bytes());
    descriptor[16..20].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    descriptor[20..24].copy_from_slice(&volume_blocks.to_le_bytes());
    descriptor[24..32].copy_from_slice(&backing_blocks.to_le_bytes());
    descriptor[32..40].copy_from_slice(&generation.to_le_bytes());
    descriptor[48..52].copy_from_slice(&(layout.log_start - 1).to_le_bytes());

    let (physical_map_start, checksum_map_start) = match slot {
        CheckpointSlot::Green => (
            layout.green_physical_map_start,
            layout.green_checksum_map_start,
        ),
        CheckpointSlot::Blue => (
            layout.blue_physical_map_start,
            layout.blue_checksum_map_start,
        ),
    };
    descriptor[52..56].copy_from_slice(&physical_map_start.to_le_bytes());
    descriptor[56..60].copy_from_slice(&layout.physical_map_blocks.to_le_bytes());
    descriptor[60..64].copy_from_slice(&checksum_map_start.to_le_bytes());
    descriptor[64..68].copy_from_slice(&layout.checksum_map_blocks.to_le_bytes());
    descriptor[68] = 1;

    let checksum = xxh3_64(descriptor);
    descriptor[DESCRIPTOR_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
}

fn submit_exact(
    ring: &mut IoUring,
    entry: squeue::Entry,
    user_data: u64,
    expected_result: i32,
) -> io::Result<()> {
    // SAFETY: each caller keeps operation buffers and the file alive until this waits for the CQE.
    unsafe {
        ring.submission()
            .push(&entry.user_data(user_data))
            .map_err(|_| io::Error::other("io_uring submission queue is full"))?;
    }
    if ring.submit_and_wait(1)? != 1 {
        return Err(io::Error::other("unexpected io_uring submission count"));
    }

    let mut completions = ring.completion();
    let completion = completions
        .next()
        .ok_or_else(|| io::Error::other("missing io_uring completion"))?;
    if completion.user_data() != user_data || completion.flags() != 0 {
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
    Ok(())
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

    let mut volume_id_bytes = [0; size_of::<u64>()];
    File::open("/dev/urandom")?.read_exact(&mut volume_id_bytes)?;
    let volume_id = u64::from_le_bytes(volume_id_bytes);

    let mut descriptors = AlignedDescriptors([0; 2 * BLOCK_SIZE]);
    encode_checkpoint_descriptor(
        &mut descriptors.0[..BLOCK_SIZE],
        CheckpointSlot::Green,
        1,
        volume_id,
        volume_blocks,
        backing_blocks,
        &layout,
    );
    encode_checkpoint_descriptor(
        &mut descriptors.0[BLOCK_SIZE..],
        CheckpointSlot::Blue,
        2,
        volume_id,
        volume_blocks,
        backing_blocks,
        &layout,
    );

    let mut ring = IoUring::new(2)?;
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

    fn write_exact(ring: &mut IoUring, file: &File, block: &AlignedBlock) -> io::Result<()> {
        let entry = opcode::Write::new(
            types::Fd(file.as_raw_fd()),
            block.0.as_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(0)
        .build();
        submit_exact(ring, entry, 1, BLOCK_SIZE as i32)
    }

    fn fsync(ring: &mut IoUring, file: &File) -> io::Result<()> {
        let entry = opcode::Fsync::new(types::Fd(file.as_raw_fd())).build();
        submit_exact(ring, entry, 2, 0)
    }

    fn read_exact(ring: &mut IoUring, file: &File, block: &mut AlignedBlock) -> io::Result<()> {
        let entry = opcode::Read::new(
            types::Fd(file.as_raw_fd()),
            block.0.as_mut_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(0)
        .build();
        submit_exact(ring, entry, 3, BLOCK_SIZE as i32)
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
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
        let mut ring = IoUring::new(4)?;
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
    fn format_writes_valid_empty_checkpoint_roots() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 1025_u32;
        let backing_blocks = 14_u64;
        backing.create_sized(backing_blocks * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        let file = File::open(&backing.0)?;
        let mut descriptors = [0; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut descriptors, 0)?;
        let volume_id = read_u64(&descriptors, 8);
        let physical_map_starts = [2, 7];
        let checksum_map_starts = [4, 9];

        for index in 0..2 {
            let descriptor = &descriptors[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE];
            assert_eq!(&descriptor[..4], b"VBLC");
            assert_eq!(descriptor[4], 1);
            assert_eq!(descriptor[5], index as u8);
            assert_eq!(u16::from_le_bytes(descriptor[6..8].try_into().unwrap()), 1);
            assert_eq!(read_u64(descriptor, 8), volume_id);
            assert_eq!(read_u32(descriptor, 16), BLOCK_SIZE as u32);
            assert_eq!(read_u32(descriptor, 20), volume_blocks);
            assert_eq!(read_u64(descriptor, 24), backing_blocks);
            assert_eq!(read_u64(descriptor, 32), index as u64 + 1);
            assert_eq!(read_u64(descriptor, 40), 0);
            assert_eq!(read_u32(descriptor, 48), 11);
            assert_eq!(read_u32(descriptor, 52), physical_map_starts[index]);
            assert_eq!(read_u32(descriptor, 56), 2);
            assert_eq!(read_u32(descriptor, 60), checksum_map_starts[index]);
            assert_eq!(read_u32(descriptor, 64), 3);
            assert_eq!(descriptor[68], 1);
            assert!(
                descriptor[69..DESCRIPTOR_CHECKSUM_OFFSET]
                    .iter()
                    .all(|byte| *byte == 0)
            );

            let expected_checksum = read_u64(descriptor, DESCRIPTOR_CHECKSUM_OFFSET);
            let mut checksummed = descriptor.to_vec();
            checksummed[DESCRIPTOR_CHECKSUM_OFFSET..].fill(0);
            assert_eq!(xxh3_64(&checksummed), expected_checksum);
        }

        let mut empty_log = [1; 2 * BLOCK_SIZE];
        file.read_exact_at(&mut empty_log, 12 * BLOCK_SIZE as u64)?;
        assert!(empty_log.iter().all(|byte| *byte == 0));
        Ok(())
    }
}
