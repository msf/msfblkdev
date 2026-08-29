use super::*;
use daemon::*;

pub fn run(repo: &Path, timeout: Duration) -> io::Result<()> {
    run_at(Paths::real(repo)?, timeout)
}

pub(super) fn run_at(paths: Paths, timeout: Duration) -> io::Result<()> {
    // Nothing that needs cleanup or preservation may be created before this returns.
    let preflight = preflight(&paths, timeout)?;
    let mut evidence = Evidence::create_ublk(&paths.repo, &preflight.fio_version)?;
    println!("evidence: {}", evidence.path().display());
    let started = Instant::now();
    let geometry = Geometry::expected();
    evidence.line(&format!(
        "backing geometry: block_bytes={} volume_blocks={} record_capacity={} log_start_blocks={} backing_blocks={} backing_bytes={}",
        geometry.block_bytes,
        geometry.volume_blocks,
        geometry.record_capacity,
        geometry.log_start_blocks,
        geometry.backing_blocks,
        geometry.backing_bytes()
    ))?;

    let mut resources = Resources::default();
    let scenario = run_scenario(
        &paths,
        &preflight,
        timeout,
        &mut resources,
        &mut evidence,
        started,
    );

    let result = match scenario {
        Ok(()) => {
            match finish_success(&paths, &preflight, timeout, &mut resources, &mut evidence) {
                Ok(()) => Ok(()),
                Err(error) => combine_with_cleanup(
                    error,
                    cleanup_error(&paths, &preflight, timeout, &mut resources, &mut evidence),
                ),
            }
        }
        Err(error) => combine_with_cleanup(
            error,
            cleanup_error(&paths, &preflight, timeout, &mut resources, &mut evidence),
        ),
    };
    evidence.line(&format!("elapsed_ms: {}", started.elapsed().as_millis()))?;
    match &result {
        Ok(()) => evidence.line("result: PASS")?,
        Err(error) => evidence.line(&format!("result: FAIL {error}"))?,
    }
    result
}

fn run_scenario(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    started: Instant,
) -> io::Result<()> {
    let owned = OwnedTempDir::create(&paths.temp_root)?;
    let backing_path = owned.path().join("backing.img");
    resources.temp = Some(owned);

    let mut format = format_command(&preflight.daemon, &backing_path);
    evidence_command(evidence, "format", &format, started)?;
    let format_result = run_command_separate(
        &mut format,
        &resources
            .temp
            .as_ref()
            .unwrap()
            .path()
            .join("format.stdout"),
        &resources
            .temp
            .as_ref()
            .unwrap()
            .path()
            .join("format.stderr"),
        Instant::now() + timeout,
    )?;
    require_success("format", format_result)?;
    evidence.line(&format!(
        "result format: exit=0 at_ms={}",
        started.elapsed().as_millis()
    ))?;
    let backing = validate_backing(
        &backing_path,
        resources.temp.as_ref().unwrap(),
        Geometry::expected().backing_bytes(),
        None,
    )?;
    backing.validate(resources.temp.as_ref().unwrap())?;
    resources.backing = Some(backing);

    let daemon_stdout = resources
        .temp
        .as_ref()
        .unwrap()
        .path()
        .join("daemon.stdout");
    let daemon_stderr = resources
        .temp
        .as_ref()
        .unwrap()
        .path()
        .join("daemon.stderr");
    let stdout = create_output(&daemon_stdout)?;
    let stderr = create_output(&daemon_stderr)?;
    let mut serve = serve_command(&preflight.daemon, &resources.backing.as_ref().unwrap().path);
    evidence_command(evidence, "serve", &serve, started)?;
    serve
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    resources.child = Some(ManagedChild::spawn(&mut serve)?);
    resources.daemon_stdout = Some(daemon_stdout.clone());
    resources.daemon_stderr = Some(daemon_stderr);

    let device = poll_readiness(
        resources.child.as_mut().unwrap(),
        &daemon_stdout,
        paths,
        Instant::now() + timeout,
    )?;
    evidence.line(&format!(
        "ready: path={} id={} at_ms={}",
        device.path.as_path().display(),
        device.id(),
        started.elapsed().as_millis()
    ))?;
    resources.device = Some(device);

    let fio_stdout = resources.temp.as_ref().unwrap().path().join("fio.stdout");
    let fio_stderr = resources.temp.as_ref().unwrap().path().join("fio.stderr");
    match resources.device.as_ref().unwrap().still_matches(paths)? {
        DeviceIdentityState::Matching => {}
        DeviceIdentityState::Absent => {
            return Err(io::Error::other("recorded device disappeared before fio"));
        }
        DeviceIdentityState::IdentityChanged => {
            return Err(io::Error::other(
                "recorded device identity changed before fio",
            ));
        }
    }
    resources
        .backing
        .as_ref()
        .unwrap()
        .validate(resources.temp.as_ref().unwrap())?;
    let mut fio = sequential_fio_command(resources.device.as_ref().unwrap().path.as_path());
    evidence_command(evidence, "fio", &fio, started)?;
    let fio_result =
        run_command_separate(&mut fio, &fio_stdout, &fio_stderr, Instant::now() + timeout)?;
    require_success("fio sequential write+flush+verify", fio_result)?;
    evidence.line(&format!(
        "result fio: exit=0 at_ms={}",
        started.elapsed().as_millis()
    ))?;
    evidence_file(evidence, "fio stdout", &fio_stdout)?;
    evidence_file(evidence, "fio stderr", &fio_stderr)?;

    prove_backing_lock(
        paths,
        preflight,
        resources.backing.as_ref().unwrap(),
        resources,
        evidence,
        timeout,
        started,
    )?;
    resources
        .backing
        .as_ref()
        .unwrap()
        .validate(resources.temp.as_ref().unwrap())?;
    Ok(())
}

fn finish_success(
    paths: &Paths,
    _preflight: &Preflight,
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
                "daemon exited unsuccessfully after SIGTERM: {status}"
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
    evidence.line(&format!(
        "shutdown: daemon exit=0 device ublkb{id} disappeared"
    ))?;
    log_daemon_files(evidence, resources)?;
    resources.device = None;
    resources
        .backing
        .as_ref()
        .unwrap()
        .validate(resources.temp.as_ref().unwrap())?;
    resources.backing = None;
    resources.temp.as_mut().unwrap().cleanup()?;
    resources.temp = None;
    Ok(())
}

fn prove_backing_lock(
    _paths: &Paths,
    preflight: &Preflight,
    backing: &OwnedBacking,
    resources: &Resources,
    evidence: &mut Evidence,
    timeout: Duration,
    started: Instant,
) -> io::Result<()> {
    let second_out = resources
        .temp
        .as_ref()
        .unwrap()
        .path()
        .join("second.stdout");
    let second_err = resources
        .temp
        .as_ref()
        .unwrap()
        .path()
        .join("second.stderr");
    let mut second = serve_command(&preflight.daemon, &backing.path);
    evidence_command(
        evidence,
        "second serve (expected lock failure)",
        &second,
        started,
    )?;
    let status = run_command_separate(
        &mut second,
        &second_out,
        &second_err,
        Instant::now() + timeout,
    )?;
    if status.code() != Some(1)
        || !fs::read(&second_out)?.is_empty()
        || fs::read_to_string(&second_err)? != LOCK_ERROR
    {
        return Err(io::Error::other(format!(
            "second daemon did not produce exact backing-lock failure (status {status})"
        )));
    }
    evidence.line(&format!(
        "backing lock: exact error, exit=1 at_ms={}",
        started.elapsed().as_millis()
    ))
}

fn combine_with_cleanup(error: io::Error, cleanup: io::Result<()>) -> io::Result<()> {
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup_error) => Err(io::Error::other(format!(
            "{error}; cleanup failed: {cleanup_error}"
        ))),
    }
}

fn cleanup_error(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
) -> io::Result<()> {
    cleanup_error_with(
        paths,
        preflight,
        timeout,
        resources,
        evidence,
        ValidatedDevice::still_matches,
    )
}

pub(super) fn cleanup_error_with<F>(
    paths: &Paths,
    preflight: &Preflight,
    timeout: Duration,
    resources: &mut Resources,
    evidence: &mut Evidence,
    mut identity_state: F,
) -> io::Result<()>
where
    F: FnMut(&ValidatedDevice, &Paths) -> io::Result<DeviceIdentityState>,
{
    let mut errors = Vec::new();
    let child_cleanup_failed =
        resources
            .child
            .as_mut()
            .is_some_and(|child| match child.cleanup() {
                Ok(()) => false,
                Err(error) => {
                    errors.push(format!("child group: {error}"));
                    true
                }
            });
    if child_cleanup_failed {
        if let Err(error) = log_daemon_files(evidence, resources) {
            errors.push(format!("daemon log capture: {error}"));
        }
        if let Some(temp) = resources.temp.as_ref() {
            errors.push(format!(
                "preserving owned temporary directory {}",
                temp.path().display()
            ));
        }
        preserve_temp(resources);
        return Err(io::Error::other(errors.join("; ")));
    }
    resources.child = None;

    let mut must_preserve_temp = false;
    if let Some(device) = resources.device.as_ref() {
        match identity_state(device, paths) {
            Ok(DeviceIdentityState::Matching) => {
                let mut delete = delete_command(&preflight.daemon, device.id());
                let temp = resources.temp.as_ref().unwrap().path();
                match run_command_separate(
                    &mut delete,
                    &temp.join("delete.stdout"),
                    &temp.join("delete.stderr"),
                    Instant::now() + timeout,
                )
                .and_then(|status| require_success("delete", status))
                {
                    Ok(()) => {
                        if let Err(error) =
                            wait_for_disappearance(paths, device.id(), Instant::now() + timeout)
                        {
                            must_preserve_temp = true;
                            errors.push(format!("device disappearance: {error}"));
                        }
                    }
                    Err(error) => {
                        must_preserve_temp = true;
                        errors.push(format!("device delete: {error}"));
                    }
                }
            }
            Ok(DeviceIdentityState::Absent) => {}
            Ok(DeviceIdentityState::IdentityChanged) => {
                must_preserve_temp = true;
                errors.push("recorded device identity changed; refusing delete".to_owned());
            }
            Err(error) => {
                must_preserve_temp = true;
                errors.push(format!(
                    "ambiguous device identity; refusing delete: {error}"
                ));
            }
        }
    }
    if let Err(error) = log_daemon_files(evidence, resources) {
        errors.push(format!("daemon log capture: {error}"));
    }

    if !must_preserve_temp
        && let (Some(backing), Some(temp)) = (resources.backing.as_ref(), resources.temp.as_ref())
        && let Err(error) = backing.validate(temp)
    {
        must_preserve_temp = true;
        errors.push(format!("backing validation before cleanup: {error}"));
    }
    if !must_preserve_temp
        && resources.backing.is_none()
        && let Some(temp) = resources.temp.as_ref()
    {
        match fs::symlink_metadata(temp.path().join("backing.img")) {
            Ok(_) => {
                must_preserve_temp = true;
                errors.push("unvalidated backing remains after format failure".to_owned());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                must_preserve_temp = true;
                errors.push(format!(
                    "cannot prove failed-format backing absence: {error}"
                ));
            }
        }
    }
    if must_preserve_temp {
        if let Some(temp) = resources.temp.as_ref() {
            errors.push(format!(
                "preserving owned temporary directory {}",
                temp.path().display()
            ));
        }
        preserve_temp(resources);
    } else {
        resources.backing = None;
        if let Some(temp) = resources.temp.as_mut()
            && let Err(error) = temp.cleanup()
        {
            errors.push(format!("temporary directory: {error}"));
        }
        resources.temp = None;
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

fn preserve_temp(resources: &mut Resources) {
    resources.backing = None;
    if let Some(temp) = resources.temp.as_mut() {
        temp.preserve();
    }
    resources.temp = None;
}

pub(super) fn validate_backing(
    path: &Path,
    owned: &OwnedTempDir,
    bytes: u64,
    expected_identity: Option<FileIdentity>,
) -> io::Result<OwnedBacking> {
    owned.validate()?;
    if path.parent() != Some(owned.path()) || path.file_name() != Some(OsStr::new("backing.img")) {
        return Err(io::Error::other("backing path is not the exact owned path"));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.dev() != fs::metadata(owned.path())?.dev()
        || metadata.len() != bytes
    {
        return Err(io::Error::other(
            "backing type, owner, filesystem, or size is invalid",
        ));
    }
    let identity = FileIdentity::from_metadata(&metadata);
    if expected_identity.is_some_and(|expected| expected != identity) {
        return Err(io::Error::other("backing identity changed"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    if FileIdentity::from_metadata(&file.metadata()?) != identity {
        return Err(io::Error::other(
            "opened backing identity differs from path",
        ));
    }
    Ok(OwnedBacking {
        path: path.to_owned(),
        identity,
        bytes,
    })
}
