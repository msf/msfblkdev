use super::*;
use crate::database_io::{BlockCounters, parse_block_stat};
use block_storage::io_accounting;
use commands::CapturedCommand;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
#[cfg(test)]
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

mod telemetry;

const LEDGER_NAME: &str = "io-ledger";
const WRITE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const PROFILE_ENV: &str = "BLOCK_STORAGE_SQLITE_PROFILE";
const TOOL_ROOT: &str = ".tools/sqlite-3.51.3";
const OPERATIONS: u64 = 4096;
const BLOBS: u64 = 2;
const BLOB_BYTES: u64 = 8192;
const CACHE_PAGES: u64 = 256;
const PAGE_BYTES: u64 = 4096;
const MMAP_BYTES: u64 = 0;
const SEED: &str = "0x6a09e667f3bcc909";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Profile {
    DeleteExtra,
    WalFull,
    WalNormal,
}

impl Profile {
    const ALL: [Self; 3] = [Self::DeleteExtra, Self::WalFull, Self::WalNormal];

    const fn name(self) -> &'static str {
        match self {
            Self::DeleteExtra => "delete-extra",
            Self::WalFull => "wal-full",
            Self::WalNormal => "wal-normal",
        }
    }

    const fn journal(self) -> &'static str {
        match self {
            Self::DeleteExtra => "delete",
            Self::WalFull | Self::WalNormal => "wal",
        }
    }

    const fn synchronous(self) -> u64 {
        match self {
            Self::DeleteExtra => 3,
            Self::WalFull => 2,
            Self::WalNormal => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Case {
    KvSeededRandomUpdate,
}

impl Case {
    const fn name(self) -> &'static str {
        match self {
            Self::KvSeededRandomUpdate => "kv-seeded-random-update",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LedgerCounters {
    read_bytes: u64,
    write_completed_bytes: u64,
    write_reserved_bytes: u64,
    reads: u64,
    writes: u64,
    flushes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct IoPoint {
    block: BlockCounters,
    ledger: LedgerCounters,
}

pub(super) struct State {
    profile: Profile,
    case: Case,
    ledger_path: PathBuf,
    reader: Option<io_accounting::Reader>,
    tool_root: PathBuf,
    result_path: PathBuf,
    reference_path: PathBuf,
    reference_digest: Option<String>,
    validation_count: u8,
    daemon_stops: u8,
    restart_started: Option<Instant>,
    setup_io_start: Option<IoPoint>,
    last_io_point: Option<IoPoint>,
    restart_origin: Option<IoPoint>,
    restart_first_end: Option<IoPoint>,
    final_shutdown_started: Option<Instant>,
    final_pre_stop: Option<IoPoint>,
    phases: BTreeMap<String, Value>,
    telemetry: Option<Value>,
    validations: Vec<Value>,
    mount_options: Vec<String>,
    metadata: Map<String, Value>,
    errors: Vec<String>,
    started: Instant,
    result_written: bool,
}

impl State {
    pub(super) fn new(fixture: &Path, evidence_directory: &Path) -> io::Result<Self> {
        let ledger_path = fixture.join(LEDGER_NAME);
        // SAFETY: State owns the private fixture and keeps the inode unchanged until
        // every daemon is stopped and the reader is dropped by finalize_ledger.
        let reader = unsafe { io_accounting::Reader::create(&ledger_path, WRITE_LIMIT_BYTES)? };
        let mode = fs::metadata(&ledger_path)?.permissions().mode() & 0o7777;
        if mode != 0o600 || fs::metadata(fixture)?.permissions().mode() & 0o777 != 0o700 {
            return Err(io::Error::other(
                "SQLite ledger fixture is not private mode 0700/0600",
            ));
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or_else(|| io::Error::other("crate has no repository parent"))?;
        Ok(Self {
            profile: Profile::WalFull,
            case: Case::KvSeededRandomUpdate,
            ledger_path,
            reader: Some(reader),
            tool_root: repo.join(TOOL_ROOT),
            result_path: evidence_directory.join("result.jsonl"),
            reference_path: fixture.join("reference.db"),
            reference_digest: None,
            validation_count: 0,
            daemon_stops: 0,
            restart_started: None,
            setup_io_start: Some(IoPoint::default()),
            last_io_point: None,
            restart_origin: None,
            restart_first_end: None,
            final_shutdown_started: None,
            final_pre_stop: None,
            phases: BTreeMap::new(),
            telemetry: None,
            validations: Vec::new(),
            mount_options: Vec::new(),
            metadata: Map::new(),
            errors: Vec::new(),
            started: Instant::now(),
            result_written: false,
        })
    }

    pub(super) fn ledger_path(&self) -> &Path {
        &self.ledger_path
    }

    fn tool(&self, name: &str) -> PathBuf {
        self.tool_root.join("build").join(name)
    }

    fn snapshot(&self) -> io::Result<LedgerCounters> {
        let snapshot = self
            .reader
            .as_ref()
            .ok_or_else(|| io::Error::other("ledger reader was already dropped"))?
            .snapshot()?;
        Ok(LedgerCounters {
            read_bytes: snapshot.read_completed_bytes,
            write_completed_bytes: snapshot.write_completed_bytes,
            write_reserved_bytes: snapshot.write_reserved_bytes,
            reads: snapshot.read_completed_operations,
            writes: snapshot.write_completed_operations,
            flushes: snapshot.flush_completed_operations,
        })
    }

    fn validate_final_ledger(&mut self) -> io::Result<Value> {
        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| io::Error::other("ledger reader was already dropped"))?;
        let snapshot = reader.snapshot()?;
        let valid = snapshot.write_limit_bytes == WRITE_LIMIT_BYTES
            && snapshot.write_reserved_bytes <= WRITE_LIMIT_BYTES
            && snapshot.pending_operations == 0
            && snapshot.errors == 0
            && !snapshot.completed_totals_incomplete
            && !snapshot.budget_exhausted;
        let value = json!({
            "write_limit_bytes": snapshot.write_limit_bytes,
            "write_reserved_bytes": snapshot.write_reserved_bytes,
            "write_completed_bytes": snapshot.write_completed_bytes,
            "read_completed_bytes": snapshot.read_completed_bytes,
            "read_completed_operations": snapshot.read_completed_operations,
            "write_completed_operations": snapshot.write_completed_operations,
            "flush_completed_operations": snapshot.flush_completed_operations,
            "pending_operations": snapshot.pending_operations,
            "errors": snapshot.errors,
            "completed_totals_incomplete": snapshot.completed_totals_incomplete,
            "budget_exhausted": snapshot.budget_exhausted,
            "valid": valid,
        });
        self.metadata.insert("final_ledger".into(), value.clone());
        if !valid {
            return Err(io::Error::other(
                "final SQLite I/O ledger is not clean and within budget",
            ));
        }
        self.reader = None;
        Ok(value)
    }

    fn result(&self, status: &str) -> Value {
        json!({
            "schema": "block-storage-sqlite-probe",
            "schema_version": 2,
            "status": status,
            "acceptance_claim": false,
            "case": {
                "name": self.case.name(), "profile": self.profile.name(),
                "filesystem": "ext4", "repetition": 1, "repetitions": 1,
            },
            "parameters": {
                "operations": OPERATIONS, "dataset_blobs": BLOBS,
                "blob_bytes": BLOB_BYTES, "page_bytes": PAGE_BYTES,
                "transaction_size": 1, "concurrency": 1,
                "seed": SEED, "access": "seeded-random", "operation": "update",
                "multitrans": true, "blob_api": false,
                "cache_pages": CACHE_PAGES, "mmap_bytes": MMAP_BYTES,
                "wal_autocheckpoint_frames": 1000,
            },
            "cache_condition": "warm-from-initialization; no global cache changes",
            "phases": self.phases,
            "telemetry": self.telemetry,
            "validations": self.validations,
            "mount_options": self.mount_options,
            "metadata": self.metadata,
            "errors": self.errors,
            "full_case_elapsed_ns": duration_ns(self.started.elapsed()),
            "measurement_gaps": {
                "device_read_performance_claim": false,
                "latency_tail_percentiles": {"available": false, "reason": "probe records aggregate process and drain durations only"},
                "matrix_and_baseline": "not implemented by this bounded probe",
            }
        })
    }

    fn write_result(&mut self, status: &str) -> io::Result<()> {
        if self.result_written {
            return Ok(());
        }
        let value = self.result(status);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.result_path)?;
        serde_json::to_writer(&mut file, &value).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        self.result_written = true;
        Ok(())
    }
}

impl Lab<'_> {
    pub(super) fn sqlite_preflight(&mut self, repo: &Path) -> io::Result<()> {
        let commit = self.run_command(
            "sqlite-canonical-commit",
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo),
        )?;
        let dirty = self.run_command(
            "sqlite-dirty-state",
            Command::new("git")
                .args(["status", "--short"])
                .current_dir(repo),
        )?;
        let evidence_directory = self
            .sqlite_state()?
            .result_path
            .parent()
            .ok_or_else(|| io::Error::other("SQLite result has no parent"))?;
        let dirty = filter_evidence_directory(&dirty, repo, evidence_directory)?;
        let kernel = self.run_command("sqlite-kernel", Command::new("uname").arg("-srvm"))?;
        let medium = self.run_command(
            "sqlite-backing-medium",
            Command::new("stat")
                .args(["-f", "-c", "%T"])
                .arg(self.owned.path()),
        )?;
        let profiles: Vec<_> = Profile::ALL.iter().map(|profile| profile.name()).collect();
        self.sqlite_state_mut()?.metadata.insert(
            "tested_source".into(),
            json!({"commit": commit.trim(), "dirty": !dirty.trim().is_empty(), "status_short": dirty,
                   "kernel": kernel.trim(), "backing_medium": medium.trim(),
                   "geometry": {"logical_block_bytes": 4096, "physical_block_bytes": 4096,
                                "max_request_bytes": 4096, "volume_bytes": Filesystem::Ext4.volume_bytes(),
                                "backing_bytes": Filesystem::Ext4.backing_bytes()},
                   "profiles_available_for_later_matrix": profiles}),
        );
        self.validate_tools()?;
        self.build_reference()
    }

    fn validate_tools(&mut self) -> io::Result<()> {
        let state = self.sqlite_state()?;
        let manifest_path = state.tool_root.join("build-manifest.json");
        let manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path)?).map_err(io::Error::other)?;
        if manifest["sqlite_version"] != "3.51.3" {
            return Err(io::Error::other("unexpected SQLite manifest version"));
        }
        let mut actual = Map::new();
        for name in ["kvtest", "sqlite3"] {
            let path = self.sqlite_state()?.tool(name);
            let output = self.run_command(
                &format!("sha256-{name}"),
                Command::new("sha256sum").arg(&path),
            )?;
            let digest = parse_sha256sum(&output, &path)?;
            if manifest["artifacts_sha256"][name].as_str() != Some(&digest) {
                return Err(io::Error::other(format!(
                    "{name} SHA256 differs from manifest"
                )));
            }
            actual.insert(name.into(), json!({"path": path, "sha256": digest}));
        }
        let sqlite3 = self.sqlite_state()?.tool("sqlite3");
        let version = self.run_command("sqlite-version", Command::new(sqlite3).arg("--version"))?;
        self.sqlite_state_mut()?.metadata.insert(
            "tools".into(),
            json!({"manifest": manifest, "manifest_path": manifest_path, "actual": actual,
                   "sqlite3_version_output": version.trim()}),
        );
        Ok(())
    }

    fn build_reference(&mut self) -> io::Result<()> {
        let path = self.sqlite_state()?.reference_path.clone();
        let start = Instant::now();
        let mut init = kv_init_command(self.sqlite_state()?.tool("kvtest"), &path);
        set_profile(&mut init, self.sqlite_state()?.profile);
        require_success(
            self.run_captured_command("reference-init", &mut init)?,
            "reference-init",
        )?;
        let mut workload = kv_workload_command(self.sqlite_state()?.tool("kvtest"), &path);
        set_profile(&mut workload, self.sqlite_state()?.profile);
        let captured = self.run_captured_command("reference-workload", &mut workload)?;
        if !captured.success() {
            return command_failure("reference-workload", &captured);
        }
        telemetry::parse(&captured.stderr, self.sqlite_state()?.profile, false)?;
        parse_workload_stdout(&captured.stdout)?;
        let digest = self.run_sqlite_validation("reference-validation", &path)?;
        self.sqlite_state_mut()?.reference_digest = Some(digest.clone());
        self.sqlite_state_mut()?.phases.insert(
            "reference_setup".into(),
            json!({"duration_ns": duration_ns(start.elapsed()), "location": "outside_tested_filesystem", "digest": digest}),
        );
        Ok(())
    }

    pub(super) fn sqlite_populate(&mut self) -> io::Result<()> {
        let database = self.mount_path().join("work/database.db");
        let setup_ns = duration_ns(self.sqlite_state()?.started.elapsed());
        let before = self.sqlite_io_point()?;
        let setup_start = self
            .sqlite_state_mut()?
            .setup_io_start
            .take()
            .ok_or_else(|| io::Error::other("SQLite setup I/O start is absent"))?;
        let setup_io = io_delta(&setup_start, &before)?;
        self.sqlite_state_mut()?.phases.insert(
            "setup_before_initialization".into(),
            json!({"duration_ns": setup_ns, "io": setup_io}),
        );
        let start = Instant::now();
        let mut init = kv_init_command(self.sqlite_state()?.tool("kvtest"), &database);
        set_profile(&mut init, self.sqlite_state()?.profile);
        let captured = self.run_captured_command("sqlite-kv-init", &mut init)?;
        require_success(captured, "sqlite-kv-init")?;
        let after = self.sqlite_io_point()?;
        self.record_io_phase("initialization", start.elapsed(), &before, &after, None)?;

        let before = after;
        let mut workload = kv_workload_command(self.sqlite_state()?.tool("kvtest"), &database);
        set_profile(&mut workload, self.sqlite_state()?.profile);
        self.configure_sqlite_observer(&mut workload)?;
        let captured = self.run_captured_command("sqlite-workload", &mut workload)?;
        if !captured.success() {
            return command_failure("sqlite-workload", &captured);
        }
        let telemetry = telemetry::parse(&captured.stderr, self.sqlite_state()?.profile, true)?;
        let upstream = parse_workload_stdout(&captured.stdout)?;
        let after = self.sqlite_io_point()?;
        let timings = workload_timings(&telemetry, captured.elapsed)?;
        let io_segments = json!({
            "segments_are_non_overlapping": true,
            "before_tool_to_drain_start_workload": io_delta(&before, &telemetry.drain_start)?,
            "drain_start_to_drain_end": io_delta(&telemetry.drain_start, &telemetry.drain_end)?,
            "drain_end_to_parent_after_residual": io_delta(&telemetry.drain_end, &after)?,
        });
        self.sqlite_state_mut()?.phases.insert(
            "workload_plus_drain".into(),
            json!({"timings": timings, "io_segments": io_segments,
                   "completed_operations": OPERATIONS, "upstream": upstream}),
        );
        let state = self.sqlite_state_mut()?;
        state.last_io_point = Some(after);
        state.telemetry = Some(telemetry.value);
        Ok(())
    }

    pub(super) fn sqlite_verify(&mut self) -> io::Result<()> {
        let database = self.mount_path().join("work/database.db");
        let before = self.sqlite_io_point()?;
        let start = Instant::now();
        let label = if self.sqlite_state()?.validation_count == 0 {
            "validation"
        } else {
            "reopen_revalidation"
        };
        let digest = self.run_sqlite_validation(label, &database)?;
        let after = self.sqlite_io_point()?;
        let expected = self
            .sqlite_state()?
            .reference_digest
            .as_deref()
            .ok_or_else(|| io::Error::other("reference digest is absent"))?;
        if digest != expected {
            return Err(io::Error::other(
                "tested SQLite logical digest differs from reference",
            ));
        }
        self.record_io_phase(
            label,
            start.elapsed(),
            &before,
            &after,
            Some(json!({"digest_matches_reference": true})),
        )?;
        let state = self.sqlite_state_mut()?;
        state.validation_count = state.validation_count.saturating_add(1);
        if state.validation_count == 1 {
            state.restart_started = Some(Instant::now());
        } else if state.validation_count == 2 {
            state.final_shutdown_started = Some(Instant::now());
        }
        state.validations.push(json!({
            "phase": label, "integrity_check": "ok", "foreign_key_rows": 0,
            "sha3sum_schema": digest, "reference_match": true,
        }));
        Ok(())
    }

    fn run_sqlite_validation(&mut self, label: &str, database: &Path) -> io::Result<String> {
        let mut command = sqlite_validation_command(self.sqlite_state()?.tool("sqlite3"), database);
        let captured = self.run_captured_command(label, &mut command)?;
        if !captured.success() {
            return command_failure(label, &captured);
        }
        parse_validation(&captured.stdout)
    }

    pub(super) fn sqlite_mounted(&mut self, options: &str) -> io::Result<()> {
        if matches!(self.workload, Workload::RealFiles) {
            return Ok(());
        }
        let restarting = self.sqlite_state()?.restart_started.is_some();
        let second_device = if restarting {
            Some(self.sqlite_io_point()?)
        } else {
            None
        };
        let state = self.sqlite_state_mut()?;
        state.mount_options.push(options.to_owned());
        if let (Some(start), Some(second), Some(origin), Some(first_end)) = (
            state.restart_started.take(),
            second_device,
            state.restart_origin.take(),
            state.restart_first_end.take(),
        ) {
            let delta = restart_io_delta(&origin, &first_end, &second)?;
            state.phases.insert(
                "restart_and_remount".into(),
                json!({"duration_ns": duration_ns(start.elapsed()), "io": delta}),
            );
            state.last_io_point = Some(second);
        }
        Ok(())
    }

    pub(super) fn sqlite_stopping(&mut self) -> io::Result<()> {
        if matches!(self.workload, Workload::RealFiles) {
            return Ok(());
        }
        let point = self.sqlite_io_point()?;
        let state = self.sqlite_state_mut()?;
        if state.daemon_stops == 0 {
            state.restart_origin = state.last_io_point.clone();
            state.restart_first_end = Some(point);
        } else {
            state.final_pre_stop = Some(point);
        }
        Ok(())
    }

    pub(super) fn sqlite_completed_requests(
        &mut self,
        completed: request_evidence::CompletedRequests,
    ) -> io::Result<()> {
        if matches!(self.workload, Workload::RealFiles) {
            return Ok(());
        }
        let state = self.sqlite_state_mut()?;
        let point = if state.daemon_stops == 0 {
            &mut state.restart_first_end
        } else {
            &mut state.final_pre_stop
        };
        let point = point
            .as_mut()
            .ok_or_else(|| io::Error::other("pre-stop I/O sample is absent"))?;
        if completed.reads < point.block.reads
            || completed.writes < point.block.writes
            || completed.flushes < point.block.flushes
        {
            return Err(io::Error::other(
                "daemon completed fewer requests than the kernel sample",
            ));
        }
        // Include requests completed after the last sysfs sample, before removal.
        point.block = BlockCounters {
            reads: completed.reads,
            writes: completed.writes,
            flushes: completed.flushes,
        };
        Ok(())
    }

    pub(super) fn sqlite_stopped(&mut self) -> io::Result<()> {
        if matches!(self.workload, Workload::RealFiles) {
            return Ok(());
        }
        let state = self.sqlite_state_mut()?;
        state.daemon_stops = state.daemon_stops.saturating_add(1);
        Ok(())
    }

    pub(super) fn sqlite_finalize_ledger(&mut self) -> io::Result<()> {
        if matches!(self.workload, Workload::RealFiles) {
            return Ok(());
        }
        let final_ledger = self.sqlite_state()?.snapshot()?;
        let state = self.sqlite_state_mut()?;
        if let (Some(start), Some(origin), Some(pre_stop)) = (
            state.final_shutdown_started.take(),
            state.last_io_point.take(),
            state.final_pre_stop.take(),
        ) {
            let final_point = IoPoint {
                block: pre_stop.block,
                ledger: final_ledger,
            };
            let delta = io_delta(&origin, &final_point)?;
            state.phases.insert(
                "final_unmount_check_and_shutdown".into(),
                json!({"duration_ns": duration_ns(start.elapsed()), "io": delta}),
            );
        }
        state.validate_final_ledger()?;
        Ok(())
    }

    pub(super) fn sqlite_write_result(
        &mut self,
        status: &str,
        error: Option<&io::Error>,
    ) -> io::Result<()> {
        if matches!(self.workload, Workload::RealFiles) {
            return Ok(());
        }
        if let Some(error) = error {
            self.sqlite_state_mut()?.errors.push(error.to_string());
        }
        self.sqlite_state_mut()?.write_result(status)
    }

    fn configure_sqlite_observer(&mut self, command: &mut Command) -> io::Result<()> {
        let executable = self.executable.clone();
        if !executable.is_absolute() {
            return Err(io::Error::other("lab executable is not absolute"));
        }
        let ledger = self
            .sqlite_ledger_path()
            .ok_or_else(|| io::Error::other("SQLite ledger path is absent"))?
            .to_path_buf();
        let device_id = self.validate_device()?.path.id().to_string();
        command
            .env("BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION", "2")
            .env("BLOCK_STORAGE_LAB_EXECUTABLE", executable)
            .env("BLOCK_STORAGE_IO_LEDGER", ledger)
            .env("BLOCK_STORAGE_IO_DEVICE_ID", device_id);
        Ok(())
    }

    fn sqlite_io_point(&mut self) -> io::Result<IoPoint> {
        let device = self.validate_device()?;
        let stat = fs::read_to_string(
            Path::new("/sys/block").join(format!("ublkb{}/stat", device.path.id())),
        )?;
        Ok(IoPoint {
            block: parse_block_stat(&stat)?,
            ledger: self.sqlite_state()?.snapshot()?,
        })
    }

    fn record_io_phase(
        &mut self,
        name: &str,
        elapsed: Duration,
        before: &IoPoint,
        after: &IoPoint,
        detail: Option<Value>,
    ) -> io::Result<()> {
        let delta = io_delta(before, after)?;
        let mut value = json!({"duration_ns": duration_ns(elapsed), "io": delta});
        if let Some(detail) = detail {
            value["detail"] = detail;
        }
        let state = self.sqlite_state_mut()?;
        state.phases.insert(name.into(), value);
        state.last_io_point = Some(after.clone());
        Ok(())
    }

    fn sqlite_state(&self) -> io::Result<&State> {
        match &self.workload {
            Workload::Sqlite(state) => Ok(state),
            Workload::RealFiles => Err(io::Error::other("SQLite state is absent")),
        }
    }

    fn sqlite_state_mut(&mut self) -> io::Result<&mut State> {
        match &mut self.workload {
            Workload::Sqlite(state) => Ok(state),
            Workload::RealFiles => Err(io::Error::other("SQLite state is absent")),
        }
    }
}

pub(crate) fn run(repo: &Path, per_command: Duration, suite: Duration) -> io::Result<()> {
    let started = Instant::now();
    let (mut evidence, directory) = Evidence::create_sqlite(repo)?;
    println!("evidence: {}", directory.display());
    evidence
        .line("sqlite probe: one ext4 WAL/FULL seeded-random kvtest case; not ADR acceptance")?;
    let mut lab = match Lab::new(
        Filesystem::Ext4,
        WorkloadMode::Sqlite,
        &mut evidence,
        per_command,
        started + suite,
        started,
    ) {
        Ok(lab) => lab,
        Err(error) => {
            write_fallback_result(&directory, started.elapsed(), &error)?;
            return Err(error);
        }
    };
    let execution = lab.preflight(repo).and_then(|()| lab.lifecycle());
    match execution {
        Ok(()) => {
            let cleanup_started = Instant::now();
            match lab.finish() {
                Ok(()) => {
                    lab.sqlite_state_mut()?.phases.insert(
                    "cleanup".into(),
                    json!({"duration_ns": duration_ns(cleanup_started.elapsed()),
                           "io": {"available": false, "reason": "device was cleanly stopped before exact fixture cleanup"}}),
                );
                    lab.sqlite_write_result("pass", None)?;
                    lab.evidence.line("result: PASS; bounded probe only")
                }
                Err(error) => {
                    if let Err(write_error) =
                        lab.sqlite_write_result(status_for(&error), Some(&error))
                    {
                        return Err(io::Error::other(format!(
                            "{error}; preserving failed result also failed: {write_error}"
                        )));
                    }
                    Err(error)
                }
            }
        }
        Err(error) => {
            let cleanup_started = Instant::now();
            let cleanup = lab.recover_error();
            lab.sqlite_state_mut()?.phases.insert(
                "failure_cleanup".into(),
                json!({"duration_ns": duration_ns(cleanup_started.elapsed()), "completed": cleanup.is_ok()}),
            );
            if let Err(cleanup_error) = &cleanup {
                lab.sqlite_state_mut()?
                    .errors
                    .push(format!("cleanup: {cleanup_error}"));
            }
            if lab.sqlite_state()?.reader.is_some()
                && lab.daemon.is_none()
                && let Err(ledger_error) = lab.sqlite_finalize_ledger()
            {
                lab.sqlite_state_mut()?
                    .errors
                    .push(format!("ledger: {ledger_error}"));
                lab.sqlite_state_mut()?.reader = None;
            }
            lab.sqlite_write_result(status_for(&error), Some(&error))?;
            Err(error)
        }
    }
}

fn kv_init_command(tool: PathBuf, database: &Path) -> Command {
    let mut command = Command::new(tool);
    command.arg("init").arg(database).args([
        "--count",
        "2",
        "--size",
        "8192",
        "--pagesize",
        "4096",
    ]);
    command
}

fn kv_workload_command(tool: PathBuf, database: &Path) -> Command {
    let mut command = Command::new(tool);
    command.arg("run").arg(database).args([
        "--update",
        "--count",
        "4096",
        "--max-id",
        "2",
        "--random",
        "--multitrans",
        "--cache-size",
        "256",
        "--mmap",
        "0",
        "--stats",
    ]);
    command
}

fn sqlite_validation_command(tool: PathBuf, database: &Path) -> Command {
    let mut command = Command::new(tool);
    command
        .args(["-batch", "-bail", "-init", "/dev/null", "-readonly"])
        .arg(database)
        .args([
            "PRAGMA integrity_check;",
            "PRAGMA foreign_key_check;",
            ".sha3sum --schema",
        ]);
    command
}

fn set_profile(command: &mut Command, profile: Profile) {
    command.env(PROFILE_ENV, profile.name());
}

pub(super) fn configure_ledger_env(command: &mut Command, ledger: Option<&Path>) {
    command.env_remove("BLOCK_STORAGE_IO_LEDGER");
    if let Some(path) = ledger {
        command.env("BLOCK_STORAGE_IO_LEDGER", path);
    }
}

fn require_success(captured: CapturedCommand, label: &str) -> io::Result<CapturedCommand> {
    if captured.success() {
        Ok(captured)
    } else {
        command_failure(label, &captured)
    }
}

fn command_failure<T>(label: &str, captured: &CapturedCommand) -> io::Result<T> {
    if !captured.completed_before_deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{label} exceeded its deadline"),
        ));
    }
    match &captured.outcome {
        Outcome::Timeout => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{label} timed out"),
        )),
        Outcome::Exit(status) => Err(io::Error::other(format!("{label} failed: {status}"))),
    }
}

fn parse_workload_stdout(bytes: &[u8]) -> io::Result<Value> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    for required in [
        "--count 4096 --max-id 2 --random",
        "--cache-size 256 --jmode wal",
        "--mmap 0",
        "Database page size: 4096",
    ] {
        if !text.lines().any(|line| line == required) {
            return Err(io::Error::other(
                "kvtest output does not confirm requested parameters",
            ));
        }
    }
    if text.contains("--blob-api") || text.contains("ERROR:") {
        return Err(io::Error::other("kvtest reported an error or blob API use"));
    }
    let elapsed = prefixed_f64(text, "Total elapsed time: ")?;
    let micros = prefixed_f64(text, "Microseconds per BLOB write: ")?;
    let rate = suffixed_f64(text, "Content write rate: ", " MB/s")?;
    Ok(json!({
        "elapsed_seconds_wall_clock": elapsed,
        "microseconds_per_blob_write": micros,
        "content_write_rate_mb_s": rate,
        "statistics_raw_in_commands_log": true,
    }))
}

fn prefixed_f64(text: &str, prefix: &str) -> io::Result<f64> {
    let raw = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .ok_or_else(|| io::Error::other("required finite upstream statistic is absent"))?;
    finite_f64(raw)
}

fn suffixed_f64(text: &str, prefix: &str, suffix: &str) -> io::Result<f64> {
    let raw = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|value| value.strip_suffix(suffix))
        .ok_or_else(|| io::Error::other("required finite upstream statistic is absent"))?;
    finite_f64(raw)
}

fn finite_f64(raw: &str) -> io::Result<f64> {
    let value = raw.parse::<f64>().map_err(io::Error::other)?;
    if !value.is_finite() || value < 0.0 {
        return Err(io::Error::other(
            "upstream statistic is not finite and non-negative",
        ));
    }
    Ok(value)
}

fn parse_validation(bytes: &[u8]) -> io::Result<String> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != 2 || lines[0] != "ok" {
        return Err(io::Error::other(
            "read-only SQLite validation output is not exact",
        ));
    }
    let digest = lines[1];
    if digest.len() != 56
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::other(
            "SQLite .sha3sum --schema digest is invalid",
        ));
    }
    Ok(digest.to_owned())
}

fn filter_evidence_directory(text: &str, repo: &Path, directory: &Path) -> io::Result<String> {
    let relative = directory
        .strip_prefix(repo)
        .map_err(|_| io::Error::other("SQLite evidence is outside repository"))?;
    let own_line = format!("?? {}/", relative.display());
    Ok(text
        .lines()
        .filter(|line| *line != own_line)
        .collect::<Vec<_>>()
        .join("\n"))
}

fn parse_sha256sum(text: &str, expected_path: &Path) -> io::Result<String> {
    let (digest, path) = text
        .trim_end()
        .split_once("  ")
        .ok_or_else(|| io::Error::other("invalid sha256sum output"))?;
    if Path::new(path) != expected_path
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::other("invalid SHA256 digest or path"));
    }
    Ok(digest.to_owned())
}

fn io_delta(before: &IoPoint, after: &IoPoint) -> io::Result<Value> {
    let subtract = |after: u64, before: u64| {
        after
            .checked_sub(before)
            .ok_or_else(|| io::Error::other("I/O counter decreased within one device instance"))
    };
    Ok(json!({
        "ublk_completed_reads": subtract(after.block.reads, before.block.reads)?,
        "ublk_completed_writes": subtract(after.block.writes, before.block.writes)?,
        "ublk_completed_flushes": subtract(after.block.flushes, before.block.flushes)?,
        "backing_read_completed_bytes": subtract(after.ledger.read_bytes, before.ledger.read_bytes)?,
        "backing_write_completed_bytes": subtract(after.ledger.write_completed_bytes, before.ledger.write_completed_bytes)?,
        "backing_write_reserved_bytes": subtract(after.ledger.write_reserved_bytes, before.ledger.write_reserved_bytes)?,
        "backing_completed_reads": subtract(after.ledger.reads, before.ledger.reads)?,
        "backing_completed_writes": subtract(after.ledger.writes, before.ledger.writes)?,
        "backing_completed_flushes": subtract(after.ledger.flushes, before.ledger.flushes)?,
        "excludes_deallocation_and_host_device_amplification": true,
        "ublk_counter_source": "data sectors / 8 and flush completions; final daemon totals at shutdown",
    }))
}

fn restart_io_delta(origin: &IoPoint, first_end: &IoPoint, second: &IoPoint) -> io::Result<Value> {
    let subtract = |after: u64, before: u64| {
        after
            .checked_sub(before)
            .ok_or_else(|| io::Error::other("restart I/O counter decreased"))
    };
    let add = |first: u64, second: u64| {
        first
            .checked_add(second)
            .ok_or_else(|| io::Error::other("restart I/O counter overflow"))
    };
    Ok(json!({
        "ublk_completed_reads": add(subtract(first_end.block.reads, origin.block.reads)?, second.block.reads)?,
        "ublk_completed_writes": add(subtract(first_end.block.writes, origin.block.writes)?, second.block.writes)?,
        "ublk_completed_flushes": add(subtract(first_end.block.flushes, origin.block.flushes)?, second.block.flushes)?,
        "backing_read_completed_bytes": subtract(second.ledger.read_bytes, origin.ledger.read_bytes)?,
        "backing_write_completed_bytes": subtract(second.ledger.write_completed_bytes, origin.ledger.write_completed_bytes)?,
        "backing_write_reserved_bytes": subtract(second.ledger.write_reserved_bytes, origin.ledger.write_reserved_bytes)?,
        "backing_completed_reads": subtract(second.ledger.reads, origin.ledger.reads)?,
        "backing_completed_writes": subtract(second.ledger.writes, origin.ledger.writes)?,
        "backing_completed_flushes": subtract(second.ledger.flushes, origin.ledger.flushes)?,
        "device_instances_combined": 2,
        "excludes_deallocation_and_host_device_amplification": true,
    }))
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn workload_timings(telemetry: &telemetry::Parsed, parent_elapsed: Duration) -> io::Result<Value> {
    let workload_plus_drain_ns = telemetry
        .process_duration_ns
        .checked_sub(telemetry.snapshot_overhead_ns)
        .filter(|value| *value > 0)
        .ok_or_else(|| io::Error::other("snapshot overhead consumes process duration"))?;
    let workload_ns = workload_plus_drain_ns
        .checked_sub(telemetry.drain_close_ns)
        .filter(|value| *value > 0)
        .ok_or_else(|| io::Error::other("drain consumes workload-plus-drain duration"))?;
    Ok(json!({
        "workload_plus_drain_ns": workload_plus_drain_ns,
        "workload_ns": workload_ns,
        "selected_connection_drain_close_ns": telemetry.drain_close_ns,
        "snapshot_observer_overhead_ns": telemetry.snapshot_overhead_ns,
        "parent_process_envelope_elapsed_ns": duration_ns(parent_elapsed),
        "parent_envelope_includes_poll_quantization": true,
        "headline_uses_in_process_monotonic_clock": true,
    }))
}

fn status_for(error: &io::Error) -> &'static str {
    if error.kind() == io::ErrorKind::TimedOut {
        "timeout"
    } else {
        "failed"
    }
}

fn invalid_telemetry() -> io::Error {
    io::Error::other("SQLite telemetry has errors, missing records, or incomplete checkpoints")
}

fn write_fallback_result(directory: &Path, elapsed: Duration, error: &io::Error) -> io::Result<()> {
    let path = directory.join("result.jsonl");
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let value = json!({
        "schema": "block-storage-sqlite-probe", "schema_version": 2,
        "status": status_for(error), "acceptance_claim": false,
        "case": {"name": Case::KvSeededRandomUpdate.name(), "profile": Profile::WalFull.name(), "filesystem": "ext4", "repetition": 1},
        "full_case_elapsed_ns": duration_ns(elapsed), "errors": [error.to_string()],
        "partial": true,
    });
    serde_json::to_writer(&mut file, &value).map_err(io::Error::other)?;
    file.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn commands_and_typed_profiles_are_exact() {
        let database = Path::new("/mounted/work/database.db");
        let init = kv_init_command("kvtest".into(), database);
        assert_eq!(
            args(&init),
            [
                "init",
                "/mounted/work/database.db",
                "--count",
                "2",
                "--size",
                "8192",
                "--pagesize",
                "4096"
            ]
        );
        let workload = kv_workload_command("kvtest".into(), database);
        let arguments = args(&workload);
        assert!(arguments.contains(&OsStr::new("--multitrans")));
        assert!(!arguments.contains(&OsStr::new("--blob-api")));
        assert!(!arguments.contains(&OsStr::new("--jmode")));
        assert_eq!(Profile::WalFull.synchronous(), 2);
        assert_eq!(Profile::DeleteExtra.synchronous(), 3);
        assert_eq!(Profile::WalNormal.synchronous(), 1);
    }

    #[test]
    fn validation_parser_is_exact_and_read_only_command_is_pinned() {
        let digest = "a".repeat(56);
        assert_eq!(
            parse_validation(format!("ok\n{digest}\n").as_bytes()).unwrap(),
            digest
        );
        assert!(parse_validation(format!("ok\nrow\n{digest}\n").as_bytes()).is_err());
        let command = sqlite_validation_command("sqlite3".into(), Path::new("database.db"));
        assert_eq!(
            args(&command)[0..6],
            [
                "-batch",
                "-bail",
                "-init",
                "/dev/null",
                "-readonly",
                "database.db"
            ]
        );
    }

    #[test]
    fn block_and_ledger_deltas_are_monotonic_and_exact() {
        let stat = "7 0 56 0 11 0 88 0 0 0 0 0 0 0 0 13 0";
        assert_eq!(
            parse_block_stat(stat).unwrap(),
            BlockCounters {
                reads: 7,
                writes: 11,
                flushes: 13
            }
        );
        let before = IoPoint::default();
        let after = IoPoint {
            block: BlockCounters {
                reads: 1,
                writes: 2,
                flushes: 3,
            },
            ledger: LedgerCounters {
                write_completed_bytes: 9,
                write_reserved_bytes: 10,
                ..LedgerCounters::default()
            },
        };
        let delta = io_delta(&before, &after).unwrap();
        assert_eq!(delta["ublk_completed_flushes"], 3);
        assert_eq!(delta["backing_write_completed_bytes"], 9);
        assert!(io_delta(&after, &before).is_err());
        let first_end = IoPoint {
            block: BlockCounters {
                reads: 3,
                writes: 4,
                flushes: 5,
            },
            ledger: LedgerCounters {
                write_completed_bytes: 12,
                ..LedgerCounters::default()
            },
        };
        let second = IoPoint {
            block: BlockCounters {
                reads: 6,
                writes: 7,
                flushes: 8,
            },
            ledger: LedgerCounters {
                write_completed_bytes: 20,
                ..LedgerCounters::default()
            },
        };
        let restart = restart_io_delta(&before, &first_end, &second).unwrap();
        assert_eq!(restart["ublk_completed_reads"], 9);
        assert_eq!(restart["backing_write_completed_bytes"], 20);
    }

    #[test]
    fn process_duration_arithmetic_uses_in_process_headline() {
        let parsed = telemetry::Parsed {
            value: Value::Null,
            process_duration_ns: 1_000,
            snapshot_overhead_ns: 100,
            drain_close_ns: 200,
            drain_start: IoPoint::default(),
            drain_end: IoPoint::default(),
        };
        let timings = workload_timings(&parsed, Duration::from_nanos(1_111)).unwrap();
        assert_eq!(timings["workload_plus_drain_ns"], 900);
        assert_eq!(timings["workload_ns"], 700);
        assert_eq!(timings["parent_process_envelope_elapsed_ns"], 1_111);

        let invalid = telemetry::Parsed {
            snapshot_overhead_ns: 1_000,
            ..parsed
        };
        assert!(workload_timings(&invalid, Duration::from_nanos(1_111)).is_err());
    }

    #[test]
    fn failed_and_timeout_json_preserve_status() {
        let failed = io::Error::other("boom");
        let timeout = io::Error::new(io::ErrorKind::TimedOut, "late");
        for (label, error, expected) in [
            ("failed", failed, "failed"),
            ("timeout", timeout, "timeout"),
        ] {
            let directory =
                std::env::temp_dir().join(format!("sqlite-result-{label}-{}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            write_fallback_result(&directory, Duration::from_nanos(7), &error).unwrap();
            let line = fs::read_to_string(directory.join("result.jsonl")).unwrap();
            let value: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(value["status"], expected);
            assert_eq!(value["errors"][0], error.to_string());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn only_daemon_commands_receive_the_cumulative_budget_path() {
        let mut daemon = Command::new("daemon");
        configure_ledger_env(&mut daemon, Some(Path::new("/tmp/private/io-ledger")));
        assert!(
            daemon
                .get_envs()
                .any(|(name, value)| name == "BLOCK_STORAGE_IO_LEDGER"
                    && value == Some(OsStr::new("/tmp/private/io-ledger")))
        );
        let mut ordinary = Command::new("checker");
        ordinary.env("BLOCK_STORAGE_IO_LEDGER", "inherited");
        configure_ledger_env(&mut ordinary, None);
        assert!(
            ordinary
                .get_envs()
                .any(|(name, value)| name == "BLOCK_STORAGE_IO_LEDGER" && value.is_none())
        );
        assert_eq!(WRITE_LIMIT_BYTES, 512 * 1024 * 1024);
    }
}
