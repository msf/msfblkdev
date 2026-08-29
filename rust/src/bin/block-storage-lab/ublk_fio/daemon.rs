use super::*;

pub(super) fn preflight(paths: &Paths, timeout: Duration) -> io::Result<Preflight> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::other("ublk-fio requires Linux"));
    }
    let daemon = paths
        .current_exe
        .parent()
        .ok_or_else(|| io::Error::other("lab executable has no parent"))?
        .join("block-storage-ublk");
    let metadata = fs::symlink_metadata(&daemon).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("missing sibling executable {}: {error}", daemon.display()),
        )
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::other(format!(
            "sibling {} is not an executable regular file",
            daemon.display()
        )));
    }

    let fio = sequential_fio_command(Path::new("/nonexistent/block-storage-lab-preflight"));
    validate_fio_bounded(&fio, Instant::now() + timeout)?;
    let fio_version = fio_version_bounded(Instant::now() + timeout)?;

    let control_metadata = fs::symlink_metadata(&paths.control).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{} must be the ublk control character device and open read/write; no resources created: {error}",
                paths.control.display()
            ),
        )
    })?;
    if !control_metadata.file_type().is_char_device() {
        return Err(io::Error::other(format!(
            "{} is not the exact character device; no resources created",
            paths.control.display()
        )));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&paths.control)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{} cannot be opened read/write; no resources created: {error}",
                    paths.control.display()
                ),
            )
        })?;
    Ok(Preflight {
        daemon,
        fio_version,
    })
}

pub(super) fn poll_readiness(
    child: &mut ManagedChild,
    stdout_path: &Path,
    paths: &Paths,
    deadline: Instant,
) -> io::Result<ValidatedDevice> {
    poll_readiness_with(child, stdout_path, paths, deadline, |paths, device| {
        ublk::validate_device_identity(&paths.dev_root, &paths.sys_root, device)
    })
}

pub(super) fn poll_readiness_with<F>(
    child: &mut ManagedChild,
    stdout_path: &Path,
    paths: &Paths,
    deadline: Instant,
    mut validate: F,
) -> io::Result<ValidatedDevice>
where
    F: FnMut(&Paths, &DevicePath) -> io::Result<FileIdentity>,
{
    loop {
        let bytes = fs::read(stdout_path)?;
        if bytes.contains(&b'\n') {
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "readiness is not UTF-8")
            })?;
            if text.lines().count() != 1 || !text.ends_with('\n') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "daemon stdout is not one exact flushed readiness line",
                ));
            }
            let device: DevicePath = text
                .strip_suffix('\n')
                .unwrap()
                .parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            match validate(paths, &device) {
                Ok(class_identity) => {
                    return Ok(ValidatedDevice {
                        path: device,
                        class_identity,
                    });
                }
                Err(error) if Instant::now() < deadline => {
                    if let Some(status) = child.try_wait()? {
                        return Err(early_exit(status, &error));
                    }
                }
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("device readiness timed out: {error}"),
                    ));
                }
            }
        } else if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "daemon exited before readiness: {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon readiness line timed out",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub(super) fn early_exit(status: ExitStatus, validation: &io::Error) -> io::Error {
    io::Error::other(format!(
        "daemon exited during device readiness ({status}): {validation}"
    ))
}

pub(super) fn wait_for_disappearance(paths: &Paths, id: u32, deadline: Instant) -> io::Result<()> {
    while !device_is_absent(paths, id)? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("recorded ublkb{id} did not disappear"),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

pub(super) fn device_is_absent(paths: &Paths, id: u32) -> io::Result<bool> {
    for path in [
        paths.dev_root.join(format!("ublkb{id}")),
        paths.sys_root.join(format!("block/ublkb{id}")),
        paths.sys_root.join(format!("class/ublk-char/ublkc{id}")),
    ] {
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

pub(super) fn format_command(daemon: &Path, backing: &Path) -> Command {
    let geometry = Geometry::expected();
    let mut command = Command::new(daemon);
    command.args([
        OsStr::new("format"),
        backing.as_os_str(),
        OsStr::new(&geometry.backing_bytes().to_string()),
        OsStr::new(&geometry.volume_bytes().to_string()),
    ]);
    command
}

pub(super) fn serve_command(daemon: &Path, backing: &Path) -> Command {
    let mut command = Command::new(daemon);
    command.args([OsStr::new("serve"), backing.as_os_str(), OsStr::new("-1")]);
    command
}

pub(super) fn delete_command(daemon: &Path, id: u32) -> Command {
    let mut command = Command::new(daemon);
    command.args([OsStr::new("delete"), OsStr::new(&id.to_string())]);
    command
}

pub(super) fn sequential_fio_command(target: &Path) -> Command {
    ublk::fio_command(
        target,
        "sequential",
        0x1357_9bdf,
        FioRw::Write,
        WRITE_BLOCKS * BLOCK_BYTES,
        0,
        FioOptions {
            do_verify: Some(true),
            fsync: Some(WRITE_BLOCKS),
            ..FioOptions::default()
        },
    )
}

pub(super) fn run_command_separate(
    command: &mut Command,
    stdout_path: &Path,
    stderr_path: &Path,
    deadline: Instant,
) -> io::Result<ExitStatus> {
    let stdout = create_output(stdout_path)?;
    let stderr = create_output(stderr_path)?;
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = ManagedChild::spawn(command)?;
    let status = match child.wait_bounded(deadline)? {
        Outcome::Exit(status) => status,
        Outcome::Timeout => {
            child.cleanup()?;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
    };
    child.cleanup()?;
    Ok(status)
}

pub(super) fn create_output(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

pub(super) fn require_success(label: &str, status: ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{label} failed with {status}")))
    }
}

pub(super) fn validate_fio_bounded(command: &Command, deadline: Instant) -> io::Result<()> {
    let mut parse = Command::new("fio");
    parse
        .args(["--parse-only", "--warnings-fatal"])
        .args(command.get_args())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut parse)?;
    let outcome = child.wait_bounded(deadline)?;
    child.cleanup()?;
    match outcome {
        Outcome::Exit(status) if status.success() => Ok(()),
        Outcome::Exit(status) => Err(io::Error::other(format!(
            "fio rejected command with {status}"
        ))),
        Outcome::Timeout => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "fio parse-only preflight timed out",
        )),
    }
}

pub(super) fn fio_version_bounded(deadline: Instant) -> io::Result<String> {
    let mut command = Command::new("fio");
    command
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut command)?;
    let outcome = child.wait_bounded(deadline)?;
    child.cleanup()?;
    let mut output = Vec::new();
    child
        .take_stdout()
        .ok_or_else(|| io::Error::other("fio version stdout is missing"))?
        .read_to_end(&mut output)?;
    match outcome {
        Outcome::Exit(status) if status.success() => String::from_utf8(output)
            .map(|text| text.trim().to_owned())
            .map_err(io::Error::other),
        Outcome::Exit(status) => Err(io::Error::other(format!(
            "fio --version failed with {status}"
        ))),
        Outcome::Timeout => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "fio --version preflight timed out",
        )),
    }
}

pub(super) fn command_text(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| format!("{:?}", part))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn evidence_command(
    evidence: &mut Evidence,
    label: &str,
    command: &Command,
    started: Instant,
) -> io::Result<()> {
    evidence.line(&format!(
        "command {label} at_ms={}: {}",
        started.elapsed().as_millis(),
        command_text(command)
    ))
}

pub(super) fn evidence_file(evidence: &mut Evidence, label: &str, path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    evidence.line(&format!("--- {label} ---"))?;
    for line in text.lines() {
        evidence.line(line)?;
    }
    evidence.line(&format!("--- end {label} ---"))
}

pub(super) fn log_daemon_files(evidence: &mut Evidence, resources: &Resources) -> io::Result<()> {
    if let Some(path) = resources.daemon_stdout.as_ref()
        && path.exists()
    {
        evidence_file(evidence, "daemon stdout", path)?;
    }
    if let Some(path) = resources.daemon_stderr.as_ref()
        && path.exists()
    {
        evidence_file(evidence, "daemon stderr", path)?;
    }
    Ok(())
}
