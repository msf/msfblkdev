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

fn write_exact(ring: &mut Option<IoUring>, file: &File, block: &AlignedBlock) -> io::Result<()> {
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

fn read_exact(ring: &mut Option<IoUring>, file: &File, block: &mut AlignedBlock) -> io::Result<()> {
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

mod checkpoint_replay;
mod format_geometry;
mod io_uring;
mod process_crash;
mod stale_tail;
mod volume_write;
