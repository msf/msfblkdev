use super::*;

pub(super) fn preflight(paths: &Paths, timeout: Duration) -> io::Result<Preflight> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::other("ublk-fio requires Linux"));
    }
    if !cfg!(feature = "test-failpoints") {
        return Err(io::Error::other(
            "ublk-fio requires --features test-failpoints; no resources created",
        ));
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

    validate_control_node_with(&paths.control, |path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
    })?;

    for fio in scenario::all_fio_commands(Path::new("/nonexistent/block-storage-lab-preflight")) {
        validate_fio_bounded(&fio, Instant::now() + timeout)?;
    }
    let fio_version = fio_version_bounded(Instant::now() + timeout)?;
    Ok(Preflight {
        daemon,
        fio_version,
    })
}

pub(super) fn validate_control_node_with<F>(path: &Path, open: F) -> io::Result<()>
where
    F: FnOnce(&Path) -> io::Result<File>,
{
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{} is missing; ask an operator to load the ublk driver with: sudo modprobe ublk_drv; no resources created",
                    path.display()
                ),
            ));
        }
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "cannot inspect {}: {error}; no resources created",
                    path.display()
                ),
            ));
        }
    };
    if !metadata.file_type().is_char_device() {
        return Err(io::Error::other(format!(
            "{} has the wrong node type; expected a character device; no resources created",
            path.display()
        )));
    }
    open(path).map_err(|error| {
        let owner = match (metadata.uid(), metadata.gid()) {
            (0, 0) => "0:0 (root:root)".to_owned(),
            (uid, gid) => format!("{uid}:{gid}"),
        };
        io::Error::new(
            error.kind(),
            format!(
                "{} is present but inaccessible: observed path={} mode={:04o} owner={owner}; open read/write failed: {error}; the already-built lab binary needs appropriate privilege or an operator-installed udev permission rule; no resources created",
                path.display(),
                path.display(),
                metadata.permissions().mode() & 0o7777,
            ),
        )
    })?;
    Ok(())
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
