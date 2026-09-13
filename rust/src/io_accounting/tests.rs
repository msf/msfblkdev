use super::*;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    directory: PathBuf,
    ledger: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "block-storage-io-accounting-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("unique fixture directory must be creatable");
        let ledger = directory.join("ledger");
        Self { directory, ledger }
    }

    fn create(&self, limit: u64) -> io::Result<Reader> {
        // SAFETY: Fixture owns its private directory and removes it only after handles drop.
        unsafe { Reader::create(&self.ledger, limit) }
    }

    fn open_reader(&self) -> io::Result<Reader> {
        // SAFETY: Fixture owns the ledger and tests mutate it only when no mapping is live,
        // except explicit in-module atomic corruption tests.
        unsafe { Reader::open(&self.ledger) }
    }

    fn open_writer(&self) -> io::Result<Writer> {
        // SAFETY: Fixture owns the ledger and all mapped access uses this module.
        unsafe { Writer::open(&self.ledger) }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).expect("fixture directory must be removable");
    }
}

#[test]
fn create_rejects_zero_budget_and_existing_path() {
    let fixture = Fixture::new();
    let error = fixture.create(0).err().expect("zero budget must fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let _reader = fixture.create(10).expect("ledger must be created");
    let error = fixture.create(10).err().expect("existing path must fail");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn only_one_writer_holds_the_lease() {
    let fixture = Fixture::new();
    let _reader = fixture.create(10).expect("ledger must be created");
    let _writer = fixture.open_writer().expect("first writer must open");
    let error = fixture
        .open_writer()
        .err()
        .expect("second writer must fail without waiting");
    assert_eq!(error.raw_os_error(), Some(libc::EWOULDBLOCK));
}

#[test]
fn readers_see_pending_and_exact_completions() {
    let fixture = Fixture::new();
    let creator = fixture.create(100).expect("ledger must be created");
    let observer = fixture.open_reader().expect("independent reader must open");
    let mut writer = fixture.open_writer().expect("writer must open");

    let read = writer
        .begin(Operation::Read { bytes: 7 })
        .expect("read must be admitted");
    assert_eq!(
        observer
            .snapshot()
            .expect("pending snapshot must be coherent")
            .pending_operations,
        1
    );
    read.finish(true).expect("read must complete");
    writer
        .begin(Operation::Write { bytes: 11 })
        .expect("write must be admitted")
        .finish(true)
        .expect("write must complete");
    writer
        .begin(Operation::Flush)
        .expect("flush must be admitted")
        .finish(true)
        .expect("flush must complete");

    let snapshot = creator.snapshot().expect("snapshot must succeed");
    assert_eq!(snapshot.write_reserved_bytes, 11);
    assert_eq!(snapshot.write_completed_bytes, 11);
    assert_eq!(snapshot.read_completed_bytes, 7);
    assert_eq!(snapshot.read_completed_operations, 1);
    assert_eq!(snapshot.write_completed_operations, 1);
    assert_eq!(snapshot.flush_completed_operations, 1);
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 0);
    assert!(!snapshot.completed_totals_incomplete);
}

#[test]
fn zero_length_io_is_rejected() {
    let fixture = Fixture::new();
    let _reader = fixture.create(10).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    for operation in [Operation::Read { bytes: 0 }, Operation::Write { bytes: 0 }] {
        assert_eq!(
            writer
                .begin(operation)
                .err()
                .expect("zero-length I/O must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn budget_is_reserved_before_io_and_never_refunded() {
    let fixture = Fixture::new();
    let reader = fixture.create(10).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    writer
        .begin(Operation::Write { bytes: 6 })
        .expect("write must be admitted")
        .finish(false)
        .expect("failure must be recorded");
    let error = writer
        .begin(Operation::Write { bytes: 5 })
        .err()
        .expect("over-budget write must fail");
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));

    let snapshot = reader.snapshot().expect("snapshot must succeed");
    assert_eq!(snapshot.write_reserved_bytes, 6);
    assert_eq!(snapshot.write_completed_bytes, 0);
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 1);
    assert!(snapshot.completed_totals_incomplete);
    assert!(snapshot.budget_exhausted);
}

#[test]
fn exact_budget_ceiling_is_admitted() {
    let fixture = Fixture::new();
    let reader = fixture.create(10).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    writer
        .begin(Operation::Write { bytes: 10 })
        .expect("exact ceiling must be admitted")
        .finish(true)
        .expect("write must complete");
    let snapshot = reader.snapshot().expect("snapshot must succeed");
    assert_eq!(snapshot.write_reserved_bytes, 10);
    assert_eq!(snapshot.write_completed_bytes, 10);
}

#[test]
fn dropped_guard_marks_unknown_completion_incomplete() {
    let fixture = Fixture::new();
    let reader = fixture.create(10).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    drop(
        writer
            .begin(Operation::Read { bytes: 3 })
            .expect("read must be admitted"),
    );
    let snapshot = reader.snapshot().expect("snapshot must succeed");
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 1);
    assert!(snapshot.completed_totals_incomplete);

    drop(writer);
    assert_eq!(
        fixture
            .open_writer()
            .err()
            .expect("incomplete ledger must refuse restart")
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn new_writer_refuses_abandoned_pending_work_without_repair() {
    let fixture = Fixture::new();
    let reader = fixture.create(10).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    let pending = writer
        .begin(Operation::Write { bytes: 4 })
        .expect("write must be admitted");
    std::mem::forget(pending);
    drop(writer);

    assert_eq!(
        fixture
            .open_writer()
            .err()
            .expect("pending ledger must refuse restart")
            .kind(),
        io::ErrorKind::InvalidData
    );
    let snapshot = reader
        .snapshot()
        .expect("raw evidence must remain readable");
    assert_eq!(snapshot.write_reserved_bytes, 4);
    assert_eq!(snapshot.pending_operations, 1);
    assert_eq!(snapshot.errors, 0);
    assert!(!snapshot.completed_totals_incomplete);
}

#[test]
fn new_writer_refuses_odd_epoch_without_repair() {
    let fixture = Fixture::new();
    let reader = fixture.create(10).expect("ledger must be created");
    let writer = fixture.open_writer().expect("writer must open");
    writer.mapping.ledger().epoch.store(1, Ordering::SeqCst);
    drop(writer);

    let error = fixture
        .open_writer()
        .err()
        .expect("odd epoch must refuse restart");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(reader.mapping.ledger().epoch.load(Ordering::SeqCst), 1);
}

#[test]
fn reservation_and_completion_overflow_are_fail_closed() {
    let fixture = Fixture::new();
    let reader = fixture.create(u64::MAX).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    writer
        .begin(Operation::Write { bytes: 1 })
        .expect("first write must be admitted")
        .finish(true)
        .expect("first write must complete");
    let error = writer
        .begin(Operation::Write { bytes: u64::MAX })
        .err()
        .expect("reservation overflow must fail");
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    assert!(
        reader
            .snapshot()
            .expect("snapshot must succeed")
            .budget_exhausted
    );

    writer
        .mapping
        .ledger()
        .flush_completed_operations
        .store(u64::MAX, Ordering::SeqCst);
    let pending = writer
        .begin(Operation::Flush)
        .expect("flush must be admitted");
    assert_eq!(
        pending
            .finish(true)
            .expect_err("completion overflow must fail")
            .kind(),
        io::ErrorKind::Other
    );
    let snapshot = reader.snapshot().expect("failed completion must settle");
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 1);
    assert!(snapshot.completed_totals_incomplete);

    writer
        .mapping
        .ledger()
        .errors
        .store(u64::MAX, Ordering::SeqCst);
    assert!(writer.begin(Operation::Flush).is_err());
    assert_eq!(
        reader
            .snapshot()
            .expect("snapshot must succeed")
            .pending_operations,
        0
    );
}

#[test]
fn subprocess_reader_sees_updates() {
    const CHILD_LEDGER: &str = "BLOCK_STORAGE_IO_ACCOUNTING_CHILD_LEDGER";
    if let Some(path) = std::env::var_os(CHILD_LEDGER) {
        // SAFETY: the parent owns the inode and keeps it stable while the child runs.
        let reader = unsafe { Reader::open(path) }.expect("child reader must open");
        let snapshot = reader.snapshot().expect("child snapshot must succeed");
        assert_eq!(snapshot.write_reserved_bytes, 9);
        assert_eq!(snapshot.write_completed_bytes, 9);
        return;
    }

    let fixture = Fixture::new();
    let _reader = fixture.create(10).expect("ledger must be created");
    let mut writer = fixture.open_writer().expect("writer must open");
    writer
        .begin(Operation::Write { bytes: 9 })
        .expect("write must be admitted")
        .finish(true)
        .expect("write must complete");
    let status = Command::new(std::env::current_exe().expect("test executable must be known"))
        .arg("--exact")
        .arg("io_accounting::tests::subprocess_reader_sees_updates")
        .arg("--nocapture")
        .env(CHILD_LEDGER, &fixture.ledger)
        .status()
        .expect("child reader must run");
    assert!(status.success());
}
