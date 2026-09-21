use super::*;
use crate::format_v4::{
    Geometry as Layout, HeadId,
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    heads::LogPosition,
    storage::FormatOptions,
};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use xxhash_rust::xxh3::xxh3_64;
const FORMAT_VERSION: u8 = crate::format_v4::VERSION;
const FOOTER_MAGIC: &[u8; 4] = b"VBLF";
const RECORD_KIND_WRITE: u8 = 1;
const FOOTER_LBA_OFFSET: usize = 32;
const FOOTER_PAYLOAD_CHECKSUM_OFFSET: usize = 1384;
const FOOTER_CHECKSUM_OFFSET: usize = BLOCK_SIZE - 8;
const CHECKPOINT_MAGIC: &[u8; 4] = b"VBLC";
const DESCRIPTOR_CHECKSUM_OFFSET: usize = BLOCK_SIZE - 8;
/// The smallest segment the tests use: a link slot plus seven writable blocks.
const TINY_SEGMENT_BLOCKS: u32 = 8;
/// A segment large enough for the 339-block probing window plus stale-tail fixtures.
const PROBE_SEGMENT_BLOCKS: u32 = 400;

/// Creates a backing of `segments` whole segments and formats it with that segment size.
fn format_segments(
    backing: &TemporaryBacking,
    volume_blocks: u32,
    segments: u64,
    blocks_per_segment: u32,
) -> io::Result<Layout> {
    backing.create_sized((2 + segments * u64::from(blocks_per_segment)) * BLOCK_SIZE as u64)?;
    format_with_options(
        &backing.0,
        u64::from(volume_blocks) * BLOCK_SIZE as u64,
        FormatOptions { blocks_per_segment },
    )?;
    Layout::new(
        volume_blocks,
        2 + segments * u64::from(blocks_per_segment),
        blocks_per_segment,
    )
}

/// The first writable block of a fresh volume: segment 0 holds its table page at block 2.
const FIRST_DATA_BLOCK: u32 = 3;
/// The LSN of the format-time table page; the first data record takes the next one.
const FORMAT_LSN: u64 = 1;

fn log_bytes_since_checkpoint(volume: &Volume) -> u64 {
    volume.log.blocks_since_checkpoint * BLOCK_SIZE as u64
}

fn user_position(volume: &Volume) -> LogPosition {
    volume.log.head(HeadId::User).position()
}

fn last_lsn(volume: &Volume) -> u64 {
    volume.log.next_lsn - 1
}

fn mapping_arrays(volume: &Volume) -> io::Result<(Vec<u32>, Vec<u64>)> {
    let mut physical = Vec::new();
    let mut checksums = Vec::new();
    let mut backing_io = BackingIo::new(None)?;
    let geometry = volume.log.geometry;
    for page_id in 0..geometry.page_count {
        let entries = match volume
            .mapping
            .cache
            .dirty_pages()
            .find(|(id, _)| *id == page_id)
        {
            Some((_, entries)) => entries.clone(),
            None => {
                let block = volume.log.index.snapshot_block(page_id);
                if block == 0 {
                    crate::mapping_cache::MappingEntries::zeroed()
                } else {
                    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
                    read_block_exact(&mut backing_io, &volume.backing, block, &mut bytes)?;
                    crate::format_v4::mapping_page::MappingPageRecord::decode(
                        &bytes.0,
                        &crate::format_v4::mapping_page::PageExpectation {
                            geometry,
                            volume_id: volume.log.volume_id,
                            page_id,
                            boundary: block,
                            maximum_lsn: last_lsn(volume),
                        },
                    )?
                    .entries
                }
            }
        };
        let count = geometry.page_entries(page_id)?;
        physical.extend_from_slice(&entries.physical_addr[..count]);
        checksums.extend_from_slice(&entries.payload_checksum[..count]);
    }
    Ok((physical, checksums))
}

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

fn write_exact(ring: &mut Option<IoUring>, file: &File, block: &AlignedBlock) -> io::Result<()> {
    let entry = opcode::Write::new(
        types::Fd(file.as_raw_fd()),
        block.0.as_ptr(),
        BLOCK_SIZE as u32,
    )
    .offset(0)
    .build();
    submit_exact_raw(ring, entry, 1, BLOCK_SIZE as i32)
}

fn fsync(ring: &mut Option<IoUring>, file: &File) -> io::Result<()> {
    let entry = opcode::Fsync::new(types::Fd(file.as_raw_fd())).build();
    submit_exact_raw(ring, entry, 2, 0)
}

fn read_exact(ring: &mut Option<IoUring>, file: &File, block: &mut AlignedBlock) -> io::Result<()> {
    let entry = opcode::Read::new(
        types::Fd(file.as_raw_fd()),
        block.0.as_mut_ptr(),
        BLOCK_SIZE as u32,
    )
    .offset(0)
    .build();
    submit_exact_raw(ring, entry, 3, BLOCK_SIZE as i32)
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

/// A four-block volume with 400-block segments and one raw record written after the table
/// page. Returns the layout and the position right after that record (`clear_start`).
fn prepare_footer_rejection_image() -> io::Result<(TemporaryBacking, Layout, u64)> {
    let backing = TemporaryBacking::new()?;
    let layout = format_segments(&backing, 4, 6, PROBE_SEGMENT_BLOCKS)?;
    let volume_id = formatted_volume_id(&backing)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    write_raw_record(
        &file,
        volume_id,
        FORMAT_LSN + 1,
        FIRST_DATA_BLOCK - 1,
        FIRST_DATA_BLOCK,
        0,
        0xa5,
    )?;
    file.sync_all()?;
    Ok((backing, layout, volume_id))
}

/// The block after the fixture's first raw record: its payload at 3 and footer at 4.
const REJECTION_CLEAR_START: u32 = FIRST_DATA_BLOCK + 2;

fn verify_recovery_stops_before_invalid_footer(
    backing: &TemporaryBacking,
    clear_start: u32,
) -> io::Result<()> {
    let opened = std::panic::catch_unwind(|| open(&backing.0))
        .map_err(|_| io::Error::other("public open panicked on invalid footer"))?;
    let mut volume = opened?;
    assert_eq!(last_lsn(&volume), FORMAT_LSN + 1);

    assert_eq!(user_position(&volume).boundary + 1, clear_start);
    assert_eq!(log_bytes_since_checkpoint(&volume), (2 * BLOCK_SIZE) as u64);
    assert_eq!(mapping_arrays(&volume)?.0, [clear_start - 2, 0, 0, 0]);
    assert_eq!(mapping_arrays(&volume)?.1[1..], [0, 0, 0]);

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
    let clear_start = REJECTION_CLEAR_START;
    let (backing, layout, volume_id) = prepare_footer_rejection_image()?;
    let segment_end = layout.segment_end(0)? as u32;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    file.write_all_at(
        &vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE],
        u64::from(clear_start) * BLOCK_SIZE as u64,
    )?;
    let invalid_footer_block = clear_start + 2;
    let mut footer = write_raw_record_payloads(
        &file,
        volume_id,
        FORMAT_LSN + 2,
        clear_start - 1,
        &[(1, 0x5a), (2, 0xc3)],
    )?;
    assert_eq!(read_u32(&footer, 28), invalid_footer_block);
    mutate(&mut footer, volume_id, clear_start);
    seal_raw_footer(&mut footer);
    assert_raw_footer_checksum_valid(&footer);
    file.write_all_at(&footer, u64::from(invalid_footer_block) * BLOCK_SIZE as u64)?;
    // Segment 2 is free, so sanitation never touches it.
    let preserved_block = layout.segment_start(2)?;
    file.write_all_at(
        &[0x9d; BLOCK_SIZE],
        u64::from(preserved_block) * BLOCK_SIZE as u64,
    )?;
    file.sync_all()?;
    drop(file);

    verify_recovery_stops_before_invalid_footer(&backing, clear_start)?;
    let file = OpenOptions::new().read(true).open(&backing.0)?;
    assert_eq!(
        file.metadata()?.len(),
        layout.backing_blocks * BLOCK_SIZE as u64
    );
    assert_zero_blocks(&file, clear_start, segment_end - clear_start)?;
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
    layout: Layout,
    clear_start: u32,
    later_payload_block: u32,
    backing_bytes: u64,
}

/// One valid record, then 339 stale blocks and a complete but unreachable record further
/// inside segment 0. Format 4 zeroes the whole rest of the segment.
fn prepare_invalid_gap_fixture() -> io::Result<InvalidGapFixture> {
    let backing = TemporaryBacking::new()?;
    let layout = format_segments(&backing, 2, 6, PROBE_SEGMENT_BLOCKS)?;
    let clear_start = REJECTION_CLEAR_START;
    let later_payload_block = clear_start + MAXIMUM_RECORD_BLOCKS as u32;
    assert!(u64::from(later_payload_block + 2) <= layout.segment_end(0)?);
    let backing_bytes = layout.backing_blocks * BLOCK_SIZE as u64;
    let volume_id = formatted_volume_id(&backing)?;

    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    write_raw_record(
        &file,
        volume_id,
        FORMAT_LSN + 1,
        FIRST_DATA_BLOCK - 1,
        FIRST_DATA_BLOCK,
        0,
        0xa5,
    )?;
    let stale_window = vec![0x7e; MAXIMUM_RECORD_BLOCKS as usize * BLOCK_SIZE];
    file.write_all_at(&stale_window, u64::from(clear_start) * BLOCK_SIZE as u64)?;
    write_raw_record(
        &file,
        volume_id,
        FORMAT_LSN + 3,
        later_payload_block - 1,
        later_payload_block,
        1,
        0x5a,
    )?;
    file.sync_all()?;

    Ok(InvalidGapFixture {
        backing,
        layout,
        clear_start,
        later_payload_block,
        backing_bytes,
    })
}

fn verify_cleared_window(fixture: &InvalidGapFixture) -> io::Result<()> {
    let file = OpenOptions::new().read(true).open(&fixture.backing.0)?;
    assert_eq!(file.metadata()?.len(), fixture.backing_bytes);
    let segment_end = fixture.layout.segment_end(0)? as u32;
    assert!(fixture.later_payload_block + 1 < segment_end);
    for block in fixture.clear_start..segment_end {
        let mut bytes = [0xa5; BLOCK_SIZE];
        file.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
        assert_eq!(bytes, [0; BLOCK_SIZE], "block {block}");
    }
    // The relocation head's empty segment is sanitized too; free segments stay untouched.
    let relocation_start = fixture.layout.segment_start(1)?;
    assert_zero_blocks(&file, relocation_start, PROBE_SEGMENT_BLOCKS)?;
    Ok(())
}

fn verify_invalid_gap_recovery(fixture: &InvalidGapFixture) -> io::Result<()> {
    let mut volume = open(&fixture.backing.0)?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, [0xa5; BLOCK_SIZE]);
    volume.read_block(1, &mut actual)?;
    assert_eq!(actual, [0; BLOCK_SIZE]);
    assert_eq!(last_lsn(&volume), FORMAT_LSN + 1);
    assert_eq!(user_position(&volume).boundary + 1, fixture.clear_start);
    drop(volume);
    verify_cleared_window(fixture)
}

mod accounting_integration;
mod checkpoint_batches;
mod checkpoint_replay;
mod format_geometry;
mod format_v4_protocol;
mod io_uring;
mod mapping_prefetch;
mod packed_writes;
mod process_crash;
mod reclaim;
mod reclaim_crash;
mod stale_tail;
mod volume_write;
mod write_buffer;

pub(crate) mod allocations;
