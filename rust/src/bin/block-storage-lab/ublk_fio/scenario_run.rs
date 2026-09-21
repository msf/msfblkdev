use super::daemon::*;
use super::lifecycle::{prove_backing_lock, validate_backing};
use super::scenario::{
    EXHAUSTION_MINIMUM_BLOCKS, FAILPOINT, FioJob, OutputNames, REPETITIONS, SCENARIOS, Scenario,
    exhaustion_verify, wrap_appended_blocks,
};
use super::*;

pub(super) fn run_all(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
) -> io::Result<()> {
    let mut lock_proven = false;
    for scenario in SCENARIOS {
        for repetition in 1..=REPETITIONS {
            evidence.line(&format!(
                "=== scenario={} repetition={repetition}/{REPETITIONS} ===",
                scenario.name()
            ))?;
            let mut outputs = OutputNames::default();
            setup_fresh(
                paths,
                preflight,
                timeout,
                resources,
                evidence,
                started,
                &mut outputs,
            )?;
            execute(
                scenario,
                paths,
                preflight,
                timeout,
                resources,
                evidence,
                started,
                &mut outputs,
            )?;
            if !lock_proven {
                prove_backing_lock(
                    preflight,
                    resources,
                    evidence,
                    timeout,
                    started,
                    &mut outputs,
                )?;
                lock_proven = true;
            }
            stop_graceful(paths, timeout, resources, evidence)?;
            resources
                .backing
                .as_ref()
                .unwrap()
                .validate(resources.temp.as_ref().unwrap())?;
            resources.backing = None;
            resources.temp.as_mut().unwrap().cleanup()?;
            resources.temp = None;
            evidence.line(&format!(
                "PASS scenario={} repetition={repetition}",
                scenario.name()
            ))?;
        }
    }
    if !lock_proven {
        return Err(io::Error::other("backing lock contention was not proved"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute(
    scenario: Scenario,
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
    outputs: &mut OutputNames,
) -> io::Result<()> {
    let jobs = scenario.fio_jobs();
    start_daemon(
        paths,
        preflight,
        timeout,
        resources,
        evidence,
        started,
        outputs,
        (scenario == Scenario::DescriptorCloseFailpoint).then_some(FAILPOINT),
    )?;
    match scenario {
        Scenario::Sequential | Scenario::Random => run_fio_success(
            paths, timeout, resources, evidence, started, outputs, jobs[0],
        ),
        Scenario::Overwrite => {
            for job in jobs {
                run_fio_success(paths, timeout, resources, evidence, started, outputs, job)?;
            }
            Ok(())
        }
        Scenario::GracefulRestart => {
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, jobs[0],
            )?;
            stop_graceful(paths, timeout, resources, evidence)?;
            start_daemon(
                paths, preflight, timeout, resources, evidence, started, outputs, None,
            )?;
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, jobs[1],
            )
        }
        Scenario::SigkillRestart => {
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, jobs[0],
            )?;
            kill_daemon(resources, timeout, evidence)?;
            remove_after_kill(paths, preflight, timeout, resources, outputs)?;
            start_daemon(
                paths, preflight, timeout, resources, evidence, started, outputs, None,
            )?;
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, jobs[1],
            )
        }
        Scenario::DescriptorCloseFailpoint => {
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, jobs[0],
            )?;
            resources.child.as_ref().unwrap().terminate()?;
            wait_for_handshake(resources, FAILPOINT, Instant::now() + timeout)?;
            evidence.line(&format!("failpoint handshake: {FAILPOINT}"))?;
            kill_daemon(resources, timeout, evidence)?;
            remove_after_kill(paths, preflight, timeout, resources, outputs)?;
            start_daemon(
                paths, preflight, timeout, resources, evidence, started, outputs, None,
            )?;
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, jobs[1],
            )
        }
        Scenario::Exhaustion => {
            let written_blocks = run_exhaustion(
                paths, timeout, resources, evidence, started, outputs, jobs[0],
            )?;
            stop_graceful(paths, timeout, resources, evidence)?;
            start_daemon(
                paths, preflight, timeout, resources, evidence, started, outputs, None,
            )?;
            run_fio_success(
                paths,
                timeout,
                resources,
                evidence,
                started,
                outputs,
                exhaustion_verify(written_blocks),
            )
        }
        Scenario::SustainedOverwrite => {
            let (passes, verify) = jobs.split_at(jobs.len() - 1);
            for job in passes {
                run_fio_success(paths, timeout, resources, evidence, started, outputs, *job)?;
            }
            stop_graceful(paths, timeout, resources, evidence)?;
            require_wraparound(evidence, resources)?;
            start_daemon(
                paths, preflight, timeout, resources, evidence, started, outputs, None,
            )?;
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, verify[0],
            )?;
            kill_daemon(resources, timeout, evidence)?;
            remove_after_kill(paths, preflight, timeout, resources, outputs)?;
            start_daemon(
                paths, preflight, timeout, resources, evidence, started, outputs, None,
            )?;
            run_fio_success(
                paths, timeout, resources, evidence, started, outputs, verify[0],
            )
        }
    }
}

/// Proves from the stopped daemon's statistics that the passes wrapped the log: more blocks
/// were appended than the log holds, and segments were freed and reused.
fn require_wraparound(evidence: &mut Evidence, resources: &Resources) -> io::Result<()> {
    let stderr = resources
        .daemon_stderr
        .as_ref()
        .ok_or_else(|| io::Error::other("daemon stderr path is missing"))?;
    let text = fs::read_to_string(stderr)?;
    let freed = scenario::statistic(&text, "reclaim_segments_freed")?;
    let appended = [
        "log_user_payload_blocks",
        "log_user_footer_blocks",
        "log_relocation_payload_blocks",
        "log_relocation_footer_blocks",
        "log_metadata_blocks",
        "log_link_blocks",
    ]
    .iter()
    .map(|name| scenario::statistic(&text, name))
    .sum::<io::Result<u64>>()?;
    let capacity = Geometry::expected().log_capacity_blocks();
    evidence.line(&format!(
        "wraparound: appended_blocks={appended} log_capacity_blocks={capacity} segments_freed={freed} expected_appended_at_least={}",
        wrap_appended_blocks()
    ))?;
    if appended < wrap_appended_blocks() || appended <= 2 * capacity || freed == 0 {
        return Err(io::Error::other(format!(
            "the passes did not wrap the log: appended_blocks={appended} capacity={capacity} segments_freed={freed}"
        )));
    }
    Ok(())
}

fn setup_fresh(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
    outputs: &mut OutputNames,
) -> io::Result<()> {
    if resources.temp.is_some() || resources.backing.is_some() || resources.child.is_some() {
        return Err(io::Error::other("previous scenario resources remain"));
    }
    let owned = OwnedTempDir::create(&paths.temp_root)?;
    owned.validate()?;
    let backing_path = owned.path().join("backing.img");
    resources.temp = Some(owned);
    let mut format = format_command(&preflight.daemon, &backing_path);
    evidence_command(evidence, "format", &format, started)?;
    let (stdout, stderr) = output_paths(resources, outputs.command("format"));
    let status = run_command_separate(&mut format, &stdout, &stderr, Instant::now() + timeout)?;
    require_success("format", status)?;
    let backing = validate_backing(
        &backing_path,
        resources.temp.as_ref().unwrap(),
        Geometry::expected().backing_bytes(),
        None,
    )?;
    backing.validate(resources.temp.as_ref().unwrap())?;
    resources.backing = Some(backing);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn start_daemon(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
    outputs: &mut OutputNames,
    failpoint: Option<&str>,
) -> io::Result<()> {
    if resources.child.is_some() || resources.device.is_some() {
        return Err(io::Error::other("daemon is already active"));
    }
    resources
        .backing
        .as_ref()
        .unwrap()
        .validate(resources.temp.as_ref().unwrap())?;
    let (stdout_path, stderr_path) = output_paths(resources, outputs.daemon());
    let mut command = serve_command(&preflight.daemon, &resources.backing.as_ref().unwrap().path);
    if let Some(name) = failpoint {
        command.env("BLOCK_STORAGE_TEST_FAILPOINT", name);
    }
    evidence_command(evidence, "serve", &command, started)?;
    command
        .stdout(Stdio::from(create_output(&stdout_path)?))
        .stderr(Stdio::from(create_output(&stderr_path)?));
    resources.child = Some(ManagedChild::spawn(&mut command)?);
    resources.daemon_stdout = Some(stdout_path.clone());
    resources.daemon_stderr = Some(stderr_path);
    let device = poll_readiness(
        resources.child.as_mut().unwrap(),
        &stdout_path,
        paths,
        Instant::now() + timeout,
    )?;
    evidence.line(&format!(
        "ready: pid={} path={} id={}",
        resources.child.as_ref().unwrap().pid(),
        device.path.as_path().display(),
        device.id()
    ))?;
    resources.device = Some(device);
    Ok(())
}

fn run_fio_success(
    paths: &Paths,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
    outputs: &mut OutputNames,
    job: FioJob,
) -> io::Result<()> {
    let (status, stdout, stderr) =
        run_fio(paths, timeout, resources, evidence, started, outputs, job)?;
    record_fio_success(evidence, job.name, status, &stdout, &stderr)
}

pub(super) fn record_fio_success(
    evidence: &mut Evidence,
    label: &str,
    status: ExitStatus,
    stdout: &Path,
    stderr: &Path,
) -> io::Result<()> {
    let stdout_result = evidence_file(evidence, "fio stdout", stdout);
    let stderr_result = evidence_file(evidence, "fio stderr", stderr);
    stdout_result?;
    stderr_result?;
    require_success(label, status)
}

/// Runs the thin-volume write until `ENOSPC` and returns how many whole blocks fio landed.
fn run_exhaustion(
    paths: &Paths,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
    outputs: &mut OutputNames,
    job: FioJob,
) -> io::Result<u64> {
    let (status, stdout, stderr) =
        run_fio(paths, timeout, resources, evidence, started, outputs, job)?;
    evidence_file(evidence, "fio exhaustion stdout", &stdout)?;
    evidence_file(evidence, "fio exhaustion stderr", &stderr)?;
    if status.code() != Some(1) {
        return Err(io::Error::other(format!(
            "exhaustion fio exit was {status}, expected 1"
        )));
    }
    let (errno, bytes) = scenario::parse_exhaustion_json(&fs::read_to_string(stdout)?)?;
    let capacity_bytes = Geometry::expected().log_capacity_blocks() * BLOCK_BYTES;
    let minimum_bytes = EXHAUSTION_MINIMUM_BLOCKS * BLOCK_BYTES;
    evidence.line(&format!(
        "exhaustion: errno={errno} io_bytes={bytes} accepted_range=[{minimum_bytes}, {capacity_bytes})"
    ))?;
    if errno != 28
        || !bytes.is_multiple_of(BLOCK_BYTES)
        || bytes < minimum_bytes
        || bytes >= capacity_bytes
    {
        return Err(io::Error::other(format!(
            "exhaustion result was errno={errno} io_bytes={bytes}, expected errno=28 and whole blocks in [{minimum_bytes}, {capacity_bytes})"
        )));
    }
    Ok(bytes / BLOCK_BYTES)
}

fn run_fio(
    paths: &Paths,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
    outputs: &mut OutputNames,
    job: FioJob,
) -> io::Result<(ExitStatus, PathBuf, PathBuf)> {
    require_matching_device(paths, resources, "before fio")?;
    resources
        .backing
        .as_ref()
        .unwrap()
        .validate(resources.temp.as_ref().unwrap())?;
    let (stdout, stderr) = output_paths(resources, outputs.fio());
    let mut fio = job.command(resources.device.as_ref().unwrap().path.as_path());
    evidence_command(evidence, "fio", &fio, started)?;
    let status = run_command_separate(&mut fio, &stdout, &stderr, Instant::now() + timeout)?;
    Ok((status, stdout, stderr))
}

fn stop_graceful(
    paths: &Paths,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
) -> io::Result<()> {
    let child = resources
        .child
        .as_mut()
        .ok_or_else(|| io::Error::other("daemon child is missing"))?;
    child.terminate()?;
    match child.wait_bounded(Instant::now() + timeout)? {
        Outcome::Exit(status) if status.success() => {}
        Outcome::Exit(status) => {
            return Err(io::Error::other(format!(
                "daemon SIGTERM exit was {status}"
            )));
        }
        Outcome::Timeout => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon SIGTERM timed out",
            ));
        }
    }
    child.cleanup()?;
    resources.child = None;
    let id = resources.device.as_ref().unwrap().id();
    wait_for_disappearance(paths, id, Instant::now() + timeout)?;
    log_daemon_files(evidence, resources)?;
    resources.device = None;
    Ok(())
}

pub(super) fn kill_daemon(
    resources: &mut Resources,
    timeout: Duration,
    evidence: &mut Evidence,
) -> io::Result<()> {
    let child = resources
        .child
        .as_mut()
        .ok_or_else(|| io::Error::other("daemon child is missing"))?;
    child.kill()?;
    match child.wait_bounded(Instant::now() + timeout)? {
        Outcome::Exit(status) if status.signal() == Some(libc::SIGKILL) => {
            evidence.line(&format!("daemon SIGKILL: signal={}", libc::SIGKILL))?;
        }
        Outcome::Exit(status) => {
            return Err(io::Error::other(format!(
                "SIGKILL daemon exit was {status}"
            )));
        }
        Outcome::Timeout => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon SIGKILL timed out",
            ));
        }
    }
    child.cleanup()?;
    resources.child = None;
    log_daemon_files(evidence, resources)
}

fn wait_for_handshake(
    resources: &mut Resources,
    expected: &str,
    deadline: Instant,
) -> io::Result<()> {
    let stdout = resources.daemon_stdout.as_ref().unwrap();
    loop {
        let text = fs::read_to_string(stdout)?;
        if text.lines().any(|line| line == expected) {
            return Ok(());
        }
        if resources.child.as_mut().unwrap().try_wait()?.is_some() {
            return Err(io::Error::other("daemon exited before failpoint handshake"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "failpoint handshake timed out",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RemoveAfterKillIdentity {
    Absent,
    Deletable,
}

pub(super) fn wait_for_remove_after_kill_identity<C, D, W>(
    preserve_for_identity: &mut bool,
    mut classify: C,
    mut deadline_reached: D,
    mut wait: W,
) -> io::Result<RemoveAfterKillIdentity>
where
    C: FnMut() -> io::Result<DeviceIdentityState>,
    D: FnMut() -> bool,
    W: FnMut(),
{
    loop {
        match classify() {
            Ok(DeviceIdentityState::Absent) => return Ok(RemoveAfterKillIdentity::Absent),
            Ok(DeviceIdentityState::StoppedMatching) => {
                return Ok(RemoveAfterKillIdentity::Deletable);
            }
            Ok(DeviceIdentityState::Matching) if deadline_reached() => {
                return Ok(RemoveAfterKillIdentity::Deletable);
            }
            Ok(DeviceIdentityState::Matching) => wait(),
            Ok(DeviceIdentityState::IdentityChanged) => {
                *preserve_for_identity = true;
                return Err(io::Error::other(
                    "recorded device identity changed after SIGKILL; refusing delete",
                ));
            }
            Err(error) if deadline_reached() => {
                *preserve_for_identity = true;
                return Err(error);
            }
            Err(_) => wait(),
        }
    }
}

fn remove_after_kill(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    outputs: &mut OutputNames,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout.min(Duration::from_secs(2));
    let id = resources.device.as_ref().unwrap().id();
    let identity = wait_for_remove_after_kill_identity(
        &mut resources.preserve_for_identity,
        || resources.device.as_ref().unwrap().still_matches(paths),
        || Instant::now() >= deadline,
        || thread::sleep(POLL_INTERVAL),
    )?;
    if identity == RemoveAfterKillIdentity::Absent {
        resources.device = None;
        return Ok(());
    }

    require_deletable_device(paths, resources, "before delete")?;
    let mut delete = delete_command(&preflight.daemon, id);
    let (stdout, stderr) = output_paths(resources, outputs.command("delete"));
    let status = run_command_separate(&mut delete, &stdout, &stderr, Instant::now() + timeout)?;
    require_success("delete old device", status)?;
    wait_for_disappearance(paths, id, Instant::now() + timeout)?;
    resources.device = None;
    Ok(())
}

fn require_matching_device(
    paths: &Paths,
    resources: &mut Resources,
    operation: &str,
) -> io::Result<()> {
    require_device_identity(paths, resources, operation, false)
}

fn require_deletable_device(
    paths: &Paths,
    resources: &mut Resources,
    operation: &str,
) -> io::Result<()> {
    require_device_identity(paths, resources, operation, true)
}

fn require_device_identity(
    paths: &Paths,
    resources: &mut Resources,
    operation: &str,
    allow_stopped: bool,
) -> io::Result<()> {
    let identity = match resources.device.as_ref().unwrap().still_matches(paths) {
        Ok(identity) => identity,
        Err(error) => {
            resources.preserve_for_identity = true;
            return Err(error);
        }
    };
    match identity {
        DeviceIdentityState::Matching => Ok(()),
        DeviceIdentityState::StoppedMatching if allow_stopped => Ok(()),
        DeviceIdentityState::StoppedMatching | DeviceIdentityState::Absent => Err(
            io::Error::other(format!("recorded device is not active {operation}")),
        ),
        DeviceIdentityState::IdentityChanged => {
            resources.preserve_for_identity = true;
            Err(io::Error::other(format!(
                "recorded device identity changed {operation}"
            )))
        }
    }
}

fn output_paths(resources: &Resources, names: (String, String)) -> (PathBuf, PathBuf) {
    let root = resources.temp.as_ref().unwrap().path();
    (root.join(names.0), root.join(names.1))
}
