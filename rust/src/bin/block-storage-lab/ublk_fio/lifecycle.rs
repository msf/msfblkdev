use super::*;
use daemon::*;

pub fn run(repo: &Path, timeout: Duration) -> io::Result<()> {
    run_at(Paths::real(repo)?, timeout)
}

pub(super) fn run_at(paths: Paths, timeout: Duration) -> io::Result<()> {
    // Preflight validates every fio shape before creating evidence or owned resources.
    let preflight = preflight(&paths, timeout)?;
    let mut evidence = Evidence::create_ublk(&paths.repo, &preflight.fio_version)?;
    println!("evidence: {}", evidence.path().display());
    let started = Instant::now();
    let geometry = Geometry::expected();
    evidence.line(&format!(
        "backing geometry: block_bytes={} volume_blocks={} record_capacity={} log_start_blocks={} backing_blocks={} backing_bytes={}",
        geometry.block_bytes, geometry.volume_blocks, geometry.record_capacity,
        geometry.log_start_blocks, geometry.backing_blocks, geometry.backing_bytes()
    ))?;

    let mut resources = Resources::default();
    let scenarios = scenario_run::run_all(
        &paths,
        &preflight,
        timeout,
        &mut resources,
        &mut evidence,
        started,
    );
    let result = match scenarios {
        Ok(()) => Ok(()),
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

pub(super) fn prove_backing_lock(
    preflight: &Preflight,
    resources: &Resources,
    evidence: &mut Evidence,
    timeout: Duration,
    started: Instant,
    outputs: &mut scenario::OutputNames,
) -> io::Result<()> {
    let backing = resources.backing.as_ref().unwrap();
    let names = outputs.command("second");
    let second_out = resources.temp.as_ref().unwrap().path().join(names.0);
    let second_err = resources.temp.as_ref().unwrap().path().join(names.1);
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

    if resources.preserve_for_identity {
        if let Err(error) = log_daemon_files(evidence, resources) {
            errors.push(format!("daemon log capture: {error}"));
        }
        errors.push("preserving backing after ambiguous or changed device identity".to_owned());
        if let Some(temp) = resources.temp.as_ref() {
            errors.push(format!(
                "preserving owned temporary directory {}",
                temp.path().display()
            ));
        }
        preserve_temp(resources);
        return Err(io::Error::other(errors.join("; ")));
    }

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
