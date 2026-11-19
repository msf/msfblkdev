use super::{Action, FaultPlan};
use crate::{
    BLOCK_SIZE, BackingIo, FormatOptions, VolumeOpenOptions,
    format_v4::{storage, volume},
    io_accounting::{Reader, Writer},
};
use io_uring::opcode;
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

const BLOCKS: usize = 2 + 12 * 8;
const VOLUME_ID: u64 = 0x51ab_0510;
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> io::Result<Self> {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("adr05-seam-{}-{id}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn image(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        File::create_new(&path)?.set_len((BLOCKS * BLOCK_SIZE) as u64)?;
        Ok(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("failed fixture: {}", self.0.display());
        } else {
            fs::remove_dir_all(&self.0).expect("remove owned test directory");
        }
    }
}

fn format(path: &Path, backing: BackingIo) -> io::Result<()> {
    storage::format_with_io(
        path,
        (8 * BLOCK_SIZE) as u64,
        FormatOptions {
            blocks_per_segment: 8,
        },
        || Ok(VOLUME_ID),
        || Ok(backing),
    )
}

#[test]
fn format_no_hit_is_byte_identical_and_enospc_stops_before_first_root() -> io::Result<()> {
    let dir = Fixture::new()?;
    let control = dir.image("control")?;
    format(&control, BackingIo::new(None)?)?;
    let expected = fs::read(&control)?;
    let image = dir.image("nohit")?;
    let mut io = BackingIo::new(None)?;
    let hits = FaultPlan::install(
        &mut io,
        opcode::Write::CODE,
        4,
        Action::Reject(libc::ENOSPC),
    );
    format(&image, io)?;
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert!(fs::read(&image)? == expected);

    let image = dir.image("reject")?;
    let mut io = BackingIo::new(None)?;
    let hits = FaultPlan::install(
        &mut io,
        opcode::Write::CODE,
        2,
        Action::Reject(libc::ENOSPC),
    );
    assert_eq!(
        format(&image, io).unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    let actual = fs::read(image)?;
    assert!(actual[..2 * BLOCK_SIZE].iter().all(|&b| b == 0));
    assert!(actual[2 * BLOCK_SIZE..] == expected[2 * BLOCK_SIZE..]);
    eprintln!("format: native == no-hit; second write ENOSPC hits=1 roots unpublished");
    Ok(())
}

#[test]
fn open_io_and_zero_byte_read_fail_without_image_changes() -> io::Result<()> {
    let dir = Fixture::new()?;
    let path = dir.image("open")?;
    format(&path, BackingIo::new(None)?)?;
    let before = fs::read(&path)?;
    for action in [Action::Reject(libc::EIO), Action::ShortZero] {
        let mut io = BackingIo::new(None)?;
        let hits = FaultPlan::install(&mut io, opcode::Read::CODE, 2, action);
        let err = volume::open_with_io(&path, VolumeOpenOptions::default(), || Ok(io))
            .err()
            .expect("second root read failed");
        match action {
            Action::Reject(_) => assert_eq!(err.raw_os_error(), Some(libc::EIO)),
            _ => assert_eq!(err.to_string(), "short io_uring operation"),
        }
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        assert!(fs::read(&path)? == before);
        eprintln!("second open read: {action:?} hits=1 no writes; {err}");
    }
    Ok(())
}

#[test]
fn writev_short_releases_no_bytes_and_charges_existing_failure_path() -> io::Result<()> {
    let dir = Fixture::new()?;
    let path = dir.image("writev")?;
    format(&path, BackingIo::new(None)?)?;
    let ledger = dir.0.join("ledger");
    // SAFETY: the test exclusively owns this ledger. No truncate or external mutations.
    let reader = unsafe { Reader::create(&ledger, 4 * 1024 * 1024)? };
    // SAFETY: same lifetime guarantee as above; only this writer mutates accounting state.
    let writer = unsafe { Writer::open(&ledger)? };
    let mut volume = crate::open_with_accounting(&path, VolumeOpenOptions::default(), writer)?;
    volume.write_block(2, &[0x22; BLOCK_SIZE])?;
    volume.flush()?;
    let before = fs::read(&path)?;
    let ledger_before = reader.snapshot()?;
    let hits = FaultPlan::install(
        &mut volume.backing_io,
        opcode::Writev::CODE,
        1,
        Action::ShortZero,
    );
    assert_eq!(
        volume
            .write_block(2, &[0x33; BLOCK_SIZE])
            .unwrap_err()
            .to_string(),
        "short io_uring operation"
    );
    assert!(volume.is_failed());
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    let after = reader.snapshot()?;
    assert_eq!(after.errors, ledger_before.errors + 1);
    assert_eq!(after.pending_operations, 0);
    assert_eq!(
        after.write_completed_bytes,
        ledger_before.write_completed_bytes
    );
    assert!(fs::read(&path)? == before);
    drop(volume);
    let mut copy = crate::open(&path)?;
    let mut data = [0; BLOCK_SIZE];
    copy.read_block(2, &mut data)?;
    assert!(data == [0x22; BLOCK_SIZE]);
    eprintln!("short writev: hits=1 ledger errors +1, pending=0, durable overwrite unchanged");
    Ok(())
}

#[test]
fn completed_fsync_can_report_uncertain_completion_and_latch_volume() -> io::Result<()> {
    let dir = Fixture::new()?;
    let path = dir.image("flush")?;
    format(&path, BackingIo::new(None)?)?;
    let mut volume = crate::open(&path)?;
    let hits = FaultPlan::install(
        &mut volume.backing_io,
        opcode::Fsync::CODE,
        2,
        Action::ErrorAfterSuccess(libc::EIO),
    );
    volume.write_block(1, &[0x55; BLOCK_SIZE])?;
    volume.flush()?;
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    volume.write_block(1, &[0x66; BLOCK_SIZE])?;
    assert_eq!(volume.flush().unwrap_err().raw_os_error(), Some(libc::EIO));
    assert!(volume.is_failed());
    assert_eq!(volume.flush().unwrap_err().to_string(), "volume failed");
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    drop(volume);
    let mut reopened = crate::open(&path)?;
    let mut data = [0; BLOCK_SIZE];
    reopened.read_block(1, &mut data)?;
    assert!(data == [0x66; BLOCK_SIZE]);
    eprintln!("second fsync: completed then reported EIO hits=1; recovered latest write");
    Ok(())
}

#[test]
fn pre_io_capacity_enospc_keeps_volume_usable() -> io::Result<()> {
    let dir = Fixture::new()?;
    let path = dir.image("capacity")?;
    File::options()
        .write(true)
        .open(&path)?
        .set_len((2 + 5 * 8) * BLOCK_SIZE as u64)?;
    format(&path, BackingIo::new(None)?)?;
    let mut volume = crate::open(&path)?;
    volume.write_block(0, &[1; BLOCK_SIZE])?;
    volume.flush()?;
    let before = fs::read(&path)?;
    let hits = FaultPlan::install(
        &mut volume.backing_io,
        opcode::Writev::CODE,
        1,
        Action::Reject(libc::EIO),
    );
    assert_eq!(
        volume
            .write_block(1, &[2; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert!(fs::read(&path)? == before);
    assert!(!volume.is_failed());
    volume.flush()?;
    eprintln!("capacity ENOSPC: backing write hits=0 image unchanged; subsequent flush ok");
    Ok(())
}

#[test]
#[ignore = "ADR05 pre-fix reproducer: backing ENOSPC during maintenance must latch"]
fn backing_enospc_at_checkpoint_b_final_fsync_must_latch() -> io::Result<()> {
    use crate::{MaintenanceStep, SelectionMode::Pressure};
    let dir = Fixture::new()?;
    let path = dir.image("maintenance")?;
    format(&path, BackingIo::new(None)?)?;
    let mut volume = crate::open(&path)?;
    for round in [1, 10] {
        for lba in 0..3 {
            volume.write_block(lba, &[(round + lba) as u8; BLOCK_SIZE])?;
        }
    }
    volume.flush()?;
    for _ in 0..16 {
        if volume.log.segments.published_free_transitions() == 1 {
            break;
        }
        if volume.maintain(Pressure)? == MaintenanceStep::Waiting {
            assert!(volume.wait_for_cleaner(std::time::Duration::from_secs(2))?);
        }
    }
    assert_eq!(
        volume.log.segments.published_free_transitions(),
        1,
        "segment pending in A"
    );
    let slot = volume.log.active_slot;
    // With A reached, one maintain runs B: sync records, write new root, sync root.
    let hits = FaultPlan::install(
        &mut volume.backing_io,
        opcode::Fsync::CODE,
        2,
        Action::Reject(libc::ENOSPC),
    );
    let err = volume.maintain(Pressure).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert_eq!(
        volume.log.active_slot, slot,
        "final sync failed before publishing B"
    );
    // Preserve the error cut before probing the next request, which can persist more bytes.
    fs::copy(&path, dir.0.join("before-next-flush"))?;
    let latched = volume.is_failed();
    let next = volume.flush();
    eprintln!("checkpoint B final fsync: {err}, hits=1 failed={latched} subsequent_flush={next:?}");
    assert!(
        latched && next.is_err(),
        "a backing failure must not bypass the volume latch"
    );
    Ok(())
}
