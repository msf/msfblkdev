use crate::evidence::Evidence;
use crate::process::{ManagedChild, Outcome};
use crate::ublk::{
    self, BLOCK_BYTES, DevicePath, FileIdentity, FioOptions, FioRw, Geometry, OwnedTempDir,
};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const LOCK_ERROR: &str = "block-storage-ublk: backing file is already locked\n";
const WRITE_BLOCKS: u64 = 16;

#[derive(Clone, Debug)]
struct Paths {
    repo: PathBuf,
    temp_root: PathBuf,
    dev_root: PathBuf,
    sys_root: PathBuf,
    control: PathBuf,
    current_exe: PathBuf,
}

impl Paths {
    fn real(repo: &Path) -> io::Result<Self> {
        Ok(Self {
            repo: repo.to_owned(),
            temp_root: std::env::temp_dir(),
            dev_root: PathBuf::from("/dev"),
            sys_root: PathBuf::from("/sys"),
            control: PathBuf::from("/dev/ublk-control"),
            current_exe: std::env::current_exe()?,
        })
    }
}

struct Preflight {
    daemon: PathBuf,
    fio_version: String,
}

struct OwnedBacking {
    path: PathBuf,
    identity: FileIdentity,
    bytes: u64,
}

impl OwnedBacking {
    fn validate(&self, owned: &OwnedTempDir) -> io::Result<()> {
        owned.validate()?;
        validate_backing(&self.path, owned, self.bytes, Some(self.identity)).map(|_| ())
    }
}

#[derive(Debug)]
struct ValidatedDevice {
    path: DevicePath,
    class_identity: FileIdentity,
}

impl ValidatedDevice {
    fn id(&self) -> u32 {
        self.path.id().get()
    }

    fn still_matches(&self, paths: &Paths) -> io::Result<bool> {
        match ublk::validate_device_identity(&paths.dev_root, &paths.sys_root, &self.path) {
            Ok(identity) => Ok(identity == self.class_identity),
            Err(validation_error) => match device_is_absent(paths, self.id()) {
                Ok(true) => Ok(false),
                Ok(false) => Err(io::Error::other(format!(
                    "cannot prove recorded device identity: {validation_error}"
                ))),
                Err(state_error) => Err(io::Error::other(format!(
                    "cannot prove recorded device identity: {validation_error}; state check failed: {state_error}"
                ))),
            },
        }
    }
}

#[derive(Default)]
struct Resources {
    child: Option<ManagedChild>,
    device: Option<ValidatedDevice>,
    temp: Option<OwnedTempDir>,
    backing: Option<OwnedBacking>,
    daemon_stdout: Option<PathBuf>,
    daemon_stderr: Option<PathBuf>,
}

#[derive(Default, Debug)]
struct CleanupProgress {
    child_group_done: bool,
    device_done: bool,
    temp_done: bool,
}

impl CleanupProgress {
    fn child_group(&mut self) {
        self.child_group_done = true;
    }

    fn device(&mut self) -> io::Result<()> {
        if !self.child_group_done {
            return Err(io::Error::other(
                "device cleanup preceded child-group cleanup",
            ));
        }
        self.device_done = true;
        Ok(())
    }

    fn temp(&mut self) -> io::Result<()> {
        if !self.device_done {
            return Err(io::Error::other("temp cleanup preceded device cleanup"));
        }
        self.temp_done = true;
        Ok(())
    }
}

pub fn run(repo: &Path, timeout: Duration) -> io::Result<()> {
    run_at(Paths::real(repo)?, timeout)
}

fn run_at(paths: Paths, timeout: Duration) -> io::Result<()> {
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

fn preflight(paths: &Paths, timeout: Duration) -> io::Result<Preflight> {
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
    if !resources.device.as_ref().unwrap().still_matches(paths)? {
        return Err(io::Error::other("recorded device disappeared before fio"));
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
    let mut progress = CleanupProgress::default();
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
    progress.child_group();

    let mut must_preserve_temp = false;
    if let Some(device) = resources.device.as_ref() {
        match device.still_matches(paths) {
            Ok(true) => {
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
            Ok(false) => {}
            Err(error) => {
                must_preserve_temp = true;
                errors.push(error.to_string());
            }
        }
    }
    progress.device()?;
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
    progress.temp()?;
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

fn validate_backing(
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

fn poll_readiness(
    child: &mut ManagedChild,
    stdout_path: &Path,
    paths: &Paths,
    deadline: Instant,
) -> io::Result<ValidatedDevice> {
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
            match ublk::validate_device_identity(&paths.dev_root, &paths.sys_root, &device) {
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

fn early_exit(status: ExitStatus, validation: &io::Error) -> io::Error {
    io::Error::other(format!(
        "daemon exited during device readiness ({status}): {validation}"
    ))
}

fn wait_for_disappearance(paths: &Paths, id: u32, deadline: Instant) -> io::Result<()> {
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

fn device_is_absent(paths: &Paths, id: u32) -> io::Result<bool> {
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

fn format_command(daemon: &Path, backing: &Path) -> Command {
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

fn serve_command(daemon: &Path, backing: &Path) -> Command {
    let mut command = Command::new(daemon);
    command.args([OsStr::new("serve"), backing.as_os_str(), OsStr::new("-1")]);
    command
}

fn delete_command(daemon: &Path, id: u32) -> Command {
    let mut command = Command::new(daemon);
    command.args([OsStr::new("delete"), OsStr::new(&id.to_string())]);
    command
}

fn sequential_fio_command(target: &Path) -> Command {
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

fn run_command_separate(
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

fn create_output(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn require_success(label: &str, status: ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{label} failed with {status}")))
    }
}

fn validate_fio_bounded(command: &Command, deadline: Instant) -> io::Result<()> {
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

fn fio_version_bounded(deadline: Instant) -> io::Result<String> {
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

fn command_text(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| format!("{:?}", part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn evidence_command(
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

fn evidence_file(evidence: &mut Evidence, label: &str, path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    evidence.line(&format!("--- {label} ---"))?;
    for line in text.lines() {
        evidence.line(line)?;
    }
    evidence.line(&format!("--- end {label} ---"))
}

fn log_daemon_files(evidence: &mut Evidence, resources: &Resources) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "block-storage-lab-lifecycle-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    fn fake_paths(root: &Path) -> Paths {
        Paths {
            repo: root.join("repo"),
            temp_root: root.join("tmp"),
            dev_root: root.join("dev"),
            sys_root: root.join("sys"),
            control: root.join("dev/ublk-control"),
            current_exe: root.join("bin/block-storage-lab"),
        }
    }

    fn current_exe_child(name: &str, env_name: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture"])
            .env(env_name, "1");
        command
    }

    fn fake_device(paths: &Paths, id: u32) {
        let queue = paths.sys_root.join(format!("block/ublkb{id}/queue"));
        let class = paths.sys_root.join(format!("class/ublk-char/ublkc{id}"));
        fs::create_dir_all(queue).unwrap();
        fs::create_dir_all(class).unwrap();
        fs::create_dir_all(&paths.dev_root).unwrap();
        fs::write(
            paths
                .sys_root
                .join(format!("block/ublkb{id}/queue/logical_block_size")),
            "4096\n",
        )
        .unwrap();
        fs::write(
            paths.sys_root.join(format!("block/ublkb{id}/size")),
            "512\n",
        )
        .unwrap();
        let loop_node = fs::read_dir("/dev")
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device()))
            .expect("test host needs one block node");
        let metadata = fs::metadata(&loop_node).unwrap();
        fs::write(
            paths.sys_root.join(format!("block/ublkb{id}/dev")),
            format!(
                "{}:{}\n",
                libc::major(metadata.rdev()),
                libc::minor(metadata.rdev())
            ),
        )
        .unwrap();
        symlink(loop_node, paths.dev_root.join(format!("ublkb{id}"))).unwrap();
    }

    #[test]
    fn command_construction_is_exact_and_bounded() {
        let daemon = Path::new("/x/block-storage-ublk");
        let backing = Path::new("/tmp/owned/backing.img");
        let format = format_command(daemon, backing);
        assert_eq!(
            format.get_args().collect::<Vec<_>>(),
            ["format", "/tmp/owned/backing.img", "286720", "262144"].map(OsStr::new)
        );
        assert_eq!(
            serve_command(daemon, backing)
                .get_args()
                .collect::<Vec<_>>(),
            ["serve", "/tmp/owned/backing.img", "-1"].map(OsStr::new)
        );
        assert_eq!(
            delete_command(daemon, 7).get_args().collect::<Vec<_>>(),
            ["delete", "7"].map(OsStr::new)
        );
        let fio = sequential_fio_command(Path::new("/dev/ublkb7"));
        let args = fio
            .get_args()
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>();
        assert!(args.contains(&"--size=65536".into()));
        assert!(args.contains(&"--fsync=16".into()));
        assert!(args.contains(&"--do_verify=1".into()));
    }

    #[test]
    fn validates_owned_backing_path_identity_and_size() {
        let root = root("backing");
        let mut owned = OwnedTempDir::create(&root).unwrap();
        let path = owned.path().join("backing.img");
        File::create(&path)
            .unwrap()
            .set_len(Geometry::expected().backing_bytes())
            .unwrap();
        let backing =
            validate_backing(&path, &owned, Geometry::expected().backing_bytes(), None).unwrap();
        backing.validate(&owned).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1)
            .unwrap();
        assert!(backing.validate(&owned).is_err());
        fs::remove_file(path).unwrap();
        owned.cleanup().unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn readiness_polls_fake_roots_and_current_exe_child() {
        let root = root("ready");
        let paths = fake_paths(&root);
        fs::create_dir_all(&paths.dev_root).unwrap();
        let stdout_path = root.join("ready.stdout");
        create_output(&stdout_path).unwrap();
        let mut command = current_exe_child(
            "ublk_fio::tests::readiness_writer_child",
            "BLOCK_STORAGE_READY_WRITER",
        );
        command
            .env("BLOCK_STORAGE_READY_PATH", &stdout_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        thread::sleep(Duration::from_millis(30));
        fake_device(&paths, 7);
        let device = poll_readiness(
            &mut child,
            &stdout_path,
            &paths,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(device.id(), 7);
        child.cleanup().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn readiness_reports_early_exit_and_timeout() {
        let root = root("ready-fail");
        let paths = fake_paths(&root);
        let early_path = root.join("early.stdout");
        create_output(&early_path).unwrap();
        let mut early =
            current_exe_child("ublk_fio::tests::empty_child", "BLOCK_STORAGE_EMPTY_CHILD");
        early.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut early).unwrap();
        let error = poll_readiness(
            &mut child,
            &early_path,
            &paths,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exited before readiness"));
        child.cleanup().unwrap();

        let timeout_path = root.join("timeout.stdout");
        create_output(&timeout_path).unwrap();
        let mut sleeping = current_exe_child(
            "ublk_fio::tests::sleeping_child",
            "BLOCK_STORAGE_READY_SLEEP",
        );
        sleeping.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut sleeping).unwrap();
        let error = poll_readiness(
            &mut child,
            &timeout_path,
            &paths,
            Instant::now() + Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        child.cleanup().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_progress_enforces_child_device_temp_order() {
        let mut progress = CleanupProgress::default();
        assert!(progress.device().is_err());
        progress.child_group();
        progress.device().unwrap();
        progress.temp().unwrap();
        assert!(progress.child_group_done && progress.device_done && progress.temp_done);
    }

    #[test]
    fn preflight_failure_creates_no_evidence_temp_or_device_resource() {
        let root = root("preflight");
        let paths = fake_paths(&root);
        fs::create_dir_all(paths.current_exe.parent().unwrap()).unwrap();
        fs::create_dir_all(&paths.repo).unwrap();
        fs::create_dir_all(&paths.temp_root).unwrap();
        fs::copy(std::env::current_exe().unwrap(), &paths.current_exe).unwrap();
        fs::copy(
            std::env::current_exe().unwrap(),
            paths
                .current_exe
                .parent()
                .unwrap()
                .join("block-storage-ublk"),
        )
        .unwrap();
        let error = run_at(paths.clone(), Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("no resources created"));
        assert!(!paths.repo.join("evidence").exists());
        assert_eq!(fs::read_dir(&paths.temp_root).unwrap().count(), 0);
        assert!(!paths.sys_root.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn readiness_writer_child() {
        if std::env::var_os("BLOCK_STORAGE_READY_WRITER").is_some() {
            fs::write(
                std::env::var_os("BLOCK_STORAGE_READY_PATH").unwrap(),
                "/dev/ublkb7\n",
            )
            .unwrap();
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn empty_child() {}

    #[test]
    fn sleeping_child() {
        if std::env::var_os("BLOCK_STORAGE_READY_SLEEP").is_some() {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
