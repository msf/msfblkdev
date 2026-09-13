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

mod hardware;
mod matrix;
pub(crate) mod params;
mod parser;
mod runner;
mod summary;
mod telemetry;

use params::{CACHE_PAGES, Case, MMAP_BYTES, PAGE_BYTES, PROFILE_ENV, Profile, SEED};
use parser::{filter_evidence_directory, parse_sha256sum, parse_validation, parse_workload_stdout};

pub(crate) use runner::{run, run_ext4};

const LEDGER_NAME: &str = "io-ledger";
const TOOL_ROOT: &str = ".tools/sqlite-3.51.3";
const WRITE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
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
    repetition: u8,
    repetitions: u8,
    matrix_run: bool,
    ledger_path: PathBuf,
    reader: Option<io_accounting::Reader>,
    tool_root: PathBuf,
    result_path: PathBuf,
    reference_path: PathBuf,
    reference_digest: Option<String>,
    reference_tool_verification: Option<String>,
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
            case: Case::ProbeSeededRandomUpdate,
            repetition: 1,
            repetitions: 1,
            matrix_run: false,
            ledger_path,
            reader: Some(reader),
            tool_root: repo.join(TOOL_ROOT),
            result_path: evidence_directory.join("result.jsonl"),
            reference_path: fixture.join("reference.db"),
            reference_digest: None,
            reference_tool_verification: None,
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

    fn select(&mut self, selection: matrix::Selection) {
        self.case = selection.case;
        self.profile = selection.profile;
        self.repetition = selection.repetition;
        self.repetitions = matrix::REPETITIONS;
        self.matrix_run = true;
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
            "schema": "block-storage-sqlite",
            "schema_version": 2,
            "scope": if self.matrix_run { "ext4_matrix" } else { "probe" },
            "status": status,
            "labels": {
                "fs": "Ext4", "case": self.case.name(), "profile": self.profile.name(),
                "repetition": self.repetition, "expected": self.repetitions,
            },
            "parameters": self.case.parameters(),
            "cache_condition": if self.case.is_speedtest() {
                "fresh database; upstream mixed workload warms caches; no global cache changes"
            } else {
                "warm-from-initialization; no global cache changes"
            },
            "phases": self.phases,
            "telemetry": self.telemetry,
            "validations": self.validations,
            "mount_options": self.mount_options,
            "metadata": self.metadata,
            "errors": self.errors,
            "full_case_elapsed_ns": duration_ns(self.started.elapsed()),
            "measurement_gaps": {
                "device_read_performance_claim": false,
                "latency_tail_percentiles": {"available": false, "reason": "upstream tools do not report latency percentiles for these cases"},
                "baseline": "no performance baseline was selected",
            }
        })
    }

    fn write_result(&mut self, status: &str) -> io::Result<()> {
        if self.result_written {
            return Ok(());
        }
        let value = self.result(status);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
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
        let profiles: Vec<_> = matrix::PROFILES
            .iter()
            .map(|profile| profile.name())
            .collect();
        self.sqlite_state_mut()?.metadata.insert(
            "tested_source".into(),
            json!({"commit": commit.trim(), "dirty": !dirty.trim().is_empty(), "status_short": dirty,
                   "kernel": kernel.trim(), "backing_medium": medium.trim(),
                   "hardware": hardware::read()?,
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
        for name in ["kvtest", "speedtest1", "sqlite3"] {
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
        let reference = self.sqlite_state()?.reference_path.clone();
        let mut version_command = Command::new(sqlite3);
        version_command.arg("--version");
        self.prepare_sqlite_command("sqlite-version", &mut version_command, &reference)?;
        let version = self.run_command("sqlite-version", &mut version_command)?;
        self.sqlite_state_mut()?.metadata.insert(
            "tools".into(),
            json!({"manifest": manifest, "manifest_path": manifest_path, "actual": actual,
                   "sqlite3_version_output": version.trim()}),
        );
        Ok(())
    }

    fn build_reference(&mut self) -> io::Result<()> {
        let path = self.sqlite_state()?.reference_path.clone();
        let case = self.sqlite_state()?.case;
        let profile = self.sqlite_state()?.profile;
        let start = Instant::now();
        if !case.is_speedtest() {
            let mut init = kv_init_command(self.sqlite_state()?.tool("kvtest"), &path, case);
            self.prepare_sqlite_command("reference-init", &mut init, &path)?;
            set_profile(&mut init, profile);
            require_success(
                self.run_captured_command("reference-init", &mut init)?,
                "reference-init",
            )?;
        }
        let mut workload = workload_command(self.sqlite_state()?, &path);
        self.prepare_sqlite_command("reference-workload", &mut workload, &path)?;
        set_profile(&mut workload, profile);
        let captured = self.run_captured_command("reference-workload", &mut workload)?;
        if !captured.success() {
            return command_failure("reference-workload", &captured);
        }
        telemetry::parse(&captured.stderr, profile, case, false)?;
        let output = parse_workload_stdout(&captured.stdout, case, profile)?;
        let digest = self.run_sqlite_validation("reference-validation", &path)?;
        let state = self.sqlite_state_mut()?;
        state.reference_digest = Some(digest.clone());
        state.reference_tool_verification = output.verification;
        state.phases.insert(
            "reference_setup".into(),
            json!({"duration_ns": duration_ns(start.elapsed()), "location": "outside_tested_filesystem", "digest": digest}),
        );
        Ok(())
    }

    pub(super) fn sqlite_populate(&mut self) -> io::Result<()> {
        let database = self.mount_path().join("work/database.db");
        let before = self.record_setup_phase()?;
        let case = self.sqlite_state()?.case;
        let profile = self.sqlite_state()?.profile;
        let before_workload = if case.is_speedtest() {
            self.sqlite_state_mut()?.phases.insert(
                "initialization".into(),
                json!({"available": false, "reason": "speedtest1 includes initialization in its measured process"}),
            );
            before
        } else {
            self.run_kv_initialization(&database, before, case, profile)?
        };
        let mut workload = workload_command(self.sqlite_state()?, &database);
        self.prepare_sqlite_command("sqlite-workload", &mut workload, &database)?;
        set_profile(&mut workload, profile);
        self.configure_sqlite_observer(&mut workload)?;
        let captured = self.run_captured_command("sqlite-workload", &mut workload)?;
        if !captured.success() {
            return command_failure("sqlite-workload", &captured);
        }
        let telemetry = telemetry::parse(&captured.stderr, profile, case, true)?;
        let output = parse_workload_stdout(&captured.stdout, case, profile)?;
        self.require_reference_tool_verification(output.verification.as_deref())?;
        self.record_workload(before_workload, captured, telemetry, output.metrics)
    }

    fn record_setup_phase(&mut self) -> io::Result<IoPoint> {
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
        Ok(before)
    }

    fn run_kv_initialization(
        &mut self,
        database: &Path,
        before: IoPoint,
        case: Case,
        profile: Profile,
    ) -> io::Result<IoPoint> {
        let start = Instant::now();
        let mut init = kv_init_command(self.sqlite_state()?.tool("kvtest"), database, case);
        self.prepare_sqlite_command("sqlite-kv-init", &mut init, database)?;
        set_profile(&mut init, profile);
        require_success(
            self.run_captured_command("sqlite-kv-init", &mut init)?,
            "sqlite-kv-init",
        )?;
        let after = self.sqlite_io_point()?;
        self.record_io_phase("initialization", start.elapsed(), &before, &after, None)?;
        Ok(after)
    }

    fn require_reference_tool_verification(&self, actual: Option<&str>) -> io::Result<()> {
        let expected = self.sqlite_state()?.reference_tool_verification.as_deref();
        if actual != expected {
            return Err(io::Error::other(
                "upstream workload verification differs from the reference",
            ));
        }
        Ok(())
    }

    fn record_workload(
        &mut self,
        before: IoPoint,
        captured: CapturedCommand,
        telemetry: telemetry::Parsed,
        upstream: Value,
    ) -> io::Result<()> {
        let after = self.sqlite_io_point()?;
        let timings = workload_timings(&telemetry, captured.elapsed)?;
        let operations = self.sqlite_state()?.case.benchmark_operations();
        let workload_ns = timings["workload_ns"]
            .as_u64()
            .ok_or_else(|| io::Error::other("workload timing is absent"))?;
        let throughput =
            operations.map(|count| count as f64 * 1_000_000_000.0 / workload_ns as f64);
        let completed = operations.map_or_else(
            || json!({"available": false, "reason": "speedtest1 reports individual mixed tests, not one operation total"}),
            |count| json!(count),
        );
        let io_segments = json!({
            "segments_are_non_overlapping": true,
            "before_tool_to_drain_start_workload": io_delta(&before, &telemetry.drain_start)?,
            "drain_start_to_drain_end": io_delta(&telemetry.drain_start, &telemetry.drain_end)?,
            "drain_end_to_parent_after_residual": io_delta(&telemetry.drain_end, &after)?,
        });
        self.sqlite_state_mut()?.phases.insert(
            "workload_plus_drain".into(),
            json!({"timings": timings, "io_segments": io_segments,
                   "completed_operations": completed,
                   "operations_per_second": throughput.map_or_else(
                       || json!({"available": false, "reason": "no meaningful aggregate operation count"}),
                       |value| json!(value)),
                   "upstream": upstream}),
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
        self.prepare_sqlite_command(label, &mut command, database)?;
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
        let connection = self.sqlite_state()?.case.snapshot_connection().to_string();
        command
            .env("BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION", connection)
            .env("BLOCK_STORAGE_LAB_EXECUTABLE", executable)
            .env("BLOCK_STORAGE_IO_LEDGER", ledger)
            .env("BLOCK_STORAGE_IO_DEVICE_ID", device_id);
        Ok(())
    }

    fn prepare_sqlite_command(
        &mut self,
        label: &str,
        command: &mut Command,
        database: &Path,
    ) -> io::Result<()> {
        let parent = prepare_sqlite_command(command, database)?;
        let location = if label.starts_with("reference-") || label == "sqlite-version" {
            "owned_fixture_outside_tested_filesystem"
        } else {
            "database_parent_on_tested_ext4_filesystem"
        };
        self.evidence.line(&format!(
            "sqlite-tool-environment: label={label} cwd={} SQLITE_TMPDIR={} database_parent={} database_parent_location={location}; cwd_and_SQLITE_TMPDIR_equal_database_parent=true",
            parent.display(),
            parent.display(),
            parent.display()
        ))
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

fn kv_init_command(tool: PathBuf, database: &Path, case: Case) -> Command {
    let mut command = Command::new(tool);
    command
        .arg("init")
        .arg(database)
        .arg("--count")
        .arg(case.dataset_blobs().to_string())
        .arg("--size")
        .arg(case.blob_bytes().to_string())
        .args(["--pagesize", "4096"]);
    command
}

fn workload_command(state: &State, database: &Path) -> Command {
    if state.case.is_speedtest() {
        let mut command = Command::new(state.tool("speedtest1"));
        command
            .arg(database)
            .args(["--testset", "main", "--size", "1", "--verify", "--stats"]);
        return command;
    }
    kv_workload_command(state.tool("kvtest"), database, state.case)
}

fn kv_workload_command(tool: PathBuf, database: &Path, case: Case) -> Command {
    let mut command = Command::new(tool);
    command
        .arg("run")
        .arg(database)
        .arg("--count")
        .arg(case.operations().to_string())
        .arg("--max-id")
        .arg(case.dataset_blobs().to_string())
        .arg(if case.random() { "--random" } else { "--asc" });
    if case.updates() {
        command.arg("--update");
    }
    if case.multitrans() {
        command.arg("--multitrans");
    }
    command.args(["--cache-size", "256", "--mmap", "0", "--stats"]);
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

fn prepare_sqlite_command<'a>(command: &mut Command, database: &'a Path) -> io::Result<&'a Path> {
    let parent = database
        .parent()
        .ok_or_else(|| io::Error::other("SQLite database has no parent directory"))?;
    if !parent.is_dir() {
        return Err(io::Error::other("SQLite database parent is absent"));
    }
    command.current_dir(parent).env("SQLITE_TMPDIR", parent);
    Ok(parent)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn commands_and_typed_profiles_are_exact() {
        let database = Path::new("/mounted/work/database.db");
        let init = kv_init_command("kvtest".into(), database, Case::ProbeSeededRandomUpdate);
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
        let workload =
            kv_workload_command("kvtest".into(), database, Case::ProbeSeededRandomUpdate);
        let arguments = args(&workload);
        assert!(arguments.contains(&OsStr::new("--multitrans")));
        assert!(!arguments.contains(&OsStr::new("--blob-api")));
        assert!(!arguments.contains(&OsStr::new("--jmode")));
        assert_eq!(Profile::WalFull.synchronous(), 2);
        assert_eq!(Profile::DeleteExtra.synchronous(), 3);
        assert_eq!(Profile::WalNormal.synchronous(), 1);
    }

    #[test]
    fn matrix_commands_cover_tools_access_sizes_and_transactions() {
        let database = Path::new("database.db");
        let speedtest = {
            let state = test_state(Case::SpeedtestMain);
            workload_command(&state, database)
        };
        assert_eq!(
            args(&speedtest),
            [
                "database.db",
                "--testset",
                "main",
                "--size",
                "1",
                "--verify",
                "--stats"
            ]
        );
        assert_eq!(speedtest.get_program(), OsStr::new("build/speedtest1"));

        let read = kv_workload_command("kvtest".into(), database, Case::KvSequentialReadSmall);
        assert!(args(&read).contains(&OsStr::new("--asc")));
        assert!(!args(&read).contains(&OsStr::new("--update")));
        assert_eq!(Case::KvSequentialReadSmall.operations(), 4096);
        assert_eq!(Case::KvSequentialReadSmall.dataset_blobs(), 32);
        assert_eq!(Case::KvSequentialReadSmall.blob_bytes(), 128);
        assert_eq!(Case::KvRandomReadLarge.blob_bytes(), 8192);
        assert_eq!(
            Case::KvSequentialUpdateSmallSingleTransaction.operations(),
            256
        );
        assert!(!Case::KvSequentialUpdateSmallSingleTransaction.multitrans());
        assert!(Case::KvSequentialUpdateSmallPerOpCommit.multitrans());
        assert_eq!(Case::KvRandomUpdateLargeSingleTransaction.operations(), 256);
        assert!(!Case::KvRandomUpdateLargeSingleTransaction.multitrans());
        assert!(Case::KvRandomUpdateLargePerOpCommit.multitrans());
        let stress =
            kv_workload_command("kvtest".into(), database, Case::KvLongWalCheckpointStress);
        assert!(args(&stress).contains(&OsStr::new("--random")));
        assert!(args(&stress).contains(&OsStr::new("--update")));
        assert!(args(&stress).contains(&OsStr::new("--multitrans")));
        assert_eq!(
            (
                Case::KvLongWalCheckpointStress.dataset_blobs(),
                Case::KvLongWalCheckpointStress.blob_bytes(),
                Case::KvLongWalCheckpointStress.operations()
            ),
            (2, 8192, 4096)
        );
    }

    fn test_state(case: Case) -> State {
        State {
            profile: Profile::WalFull,
            case,
            repetition: 1,
            repetitions: matrix::REPETITIONS,
            matrix_run: true,
            ledger_path: PathBuf::new(),
            reader: None,
            tool_root: PathBuf::new(),
            result_path: PathBuf::new(),
            reference_path: PathBuf::new(),
            reference_digest: None,
            reference_tool_verification: None,
            validation_count: 0,
            daemon_stops: 0,
            restart_started: None,
            setup_io_start: None,
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
        }
    }

    #[test]
    fn matrix_result_uses_generic_schema_and_explicit_scope() {
        let selection = matrix::selected(Case::SpeedtestMain, Profile::WalFull, 2).unwrap();
        let mut focused = test_state(Case::SpeedtestMain);
        focused.select(selection);
        let result = focused.result("pass");
        assert_eq!(result["schema"], "block-storage-sqlite");
        assert_eq!(result["schema_version"], 2);
        assert_eq!(result["scope"], "ext4_matrix");
        assert_eq!(result["labels"]["fs"], "Ext4");
        assert_eq!(result["labels"]["repetition"], 2);
        assert_eq!(result["labels"]["expected"], 3);
        assert_eq!(result["parameters"]["tool"], "speedtest1");
    }

    #[test]
    fn sqlite_database_commands_pin_tmpdir_and_database_parent() {
        let root = std::env::temp_dir().join(format!("sqlite-command-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let database = root.join("database.db");
        let mut command = sqlite_validation_command("sqlite3".into(), &database);
        prepare_sqlite_command(&mut command, &database).unwrap();
        assert_eq!(command.get_current_dir(), Some(root.as_path()));
        assert!(
            command.get_envs().any(|(name, value)| {
                name == "SQLITE_TMPDIR" && value == Some(root.as_os_str())
            })
        );
        fs::remove_dir(root).unwrap();
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
