use super::*;

impl<'a> Lab<'a> {
    pub(super) fn new(
        evidence: &'a mut Evidence,
        per_command: Duration,
        suite_deadline: Instant,
        started: Instant,
    ) -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let daemon_executable = executable
            .parent()
            .ok_or_else(|| io::Error::other("lab executable has no parent"))?
            .join("block-storage-ublk");
        let owned = OwnedTempDir::create(Path::new("/tmp"))?;
        evidence.line(&format!(
            "fixture: {} identity={}",
            owned.path().display(),
            owned.identity()
        ))?;
        Ok(Self {
            owned,
            mount_identity: None,
            backing_identity: None,
            device: None,
            daemon: None,
            command: None,
            daemon_outputs: None,
            mount_id: None,
            mount_attempted: false,
            executable,
            daemon_executable,
            evidence,
            sequence: 0,
            per_command,
            suite_deadline,
            started,
            finished: false,
        })
    }

    pub(super) fn deadline(&self) -> io::Result<Instant> {
        let now = Instant::now();
        if now >= self.suite_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ext4 suite deadline exceeded",
            ));
        }
        Ok((now + self.per_command).min(self.suite_deadline))
    }

    pub(super) fn outputs(&mut self, label: &str) -> (PathBuf, PathBuf) {
        self.sequence += 1;
        (
            self.owned
                .path()
                .join(format!("{:03}-{label}.stdout", self.sequence)),
            self.owned
                .path()
                .join(format!("{:03}-{label}.stderr", self.sequence)),
        )
    }

    pub(super) fn log_command(&mut self, label: &str, command: &Command) -> io::Result<()> {
        self.evidence.line(&format!(
            "command {label} at_ms={}: {:?} {:?}",
            self.started.elapsed().as_millis(),
            command.get_program(),
            command.get_args().collect::<Vec<_>>()
        ))
    }

    pub(super) fn capture(&mut self, label: &str, path: &Path) -> io::Result<String> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_CAPTURE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CAPTURE_BYTES {
            return Err(io::Error::other(
                "command output exceeds capture limit; full file preserved",
            ));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.evidence.line(&format!(
            "--- {label}: {} ---\n{text}\n--- end {label} ---",
            path.display()
        ))?;
        Ok(text)
    }

    pub(super) fn run_command(&mut self, label: &str, command: &mut Command) -> io::Result<String> {
        if self.command.is_some() {
            return Err(io::Error::other(
                "previous command group has not been reaped",
            ));
        }
        let deadline = self.deadline()?;
        let started = Instant::now();
        let (stdout, stderr) = self.outputs(label);
        self.log_command(label, command)?;
        command
            .stdin(Stdio::null())
            .env("LC_ALL", "C")
            .env_remove("BLOCK_STORAGE_TEST_FAILPOINT")
            .stdout(Stdio::from(new_output(&stdout)?))
            .stderr(Stdio::from(new_output(&stderr)?));
        self.command = Some(ManagedChild::spawn(command)?);
        let child = self.command.as_mut().expect("just spawned command");
        let outcome = child.wait_bounded(deadline);
        let cleanup = child.cleanup();
        if cleanup.is_ok() {
            self.command = None;
        }
        let output = self.capture("stdout", &stdout)?;
        self.capture("stderr", &stderr)?;
        self.evidence.line(&format!(
            "command {label}: outcome={outcome:?} elapsed_ms={} cleanup={cleanup:?}",
            started.elapsed().as_millis()
        ))?;
        cleanup?;
        match outcome? {
            Outcome::Exit(status) if status.success() && Instant::now() <= deadline => Ok(output),
            Outcome::Exit(status) if !status.success() => {
                Err(io::Error::other(format!("{label} failed: {status}")))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{label} exceeded command or suite deadline"),
            )),
        }
    }

    pub(super) fn preflight(&mut self, repo: &Path) -> io::Result<()> {
        // SAFETY: geteuid has no arguments or memory-safety preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return Err(io::Error::other(
                "run the ext4 lab as a normal user; only the installed mount helper uses sudo",
            ));
        }
        self.run_command(
            "commit",
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo),
        )?;
        self.run_command(
            "worktree",
            Command::new("git")
                .args(["status", "--short"])
                .current_dir(repo),
        )?;
        self.run_command("kernel", Command::new("uname").arg("-srvm"))?;
        for binary in [self.executable.clone(), self.daemon_executable.clone()] {
            let metadata = fs::symlink_metadata(&binary)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return Err(io::Error::other(
                    "expected a sibling executable regular file",
                ));
            }
            let bytes = fs::read(&binary)?;
            self.evidence.line(&format!(
                "binary: {} bytes={} xxh3={:016x}",
                binary.display(),
                bytes.len(),
                xxhash_rust::xxh3::xxh3_64(&bytes)
            ))?;
        }
        self.run_command("mkfs-version", Command::new("mkfs.ext4").arg("-V"))?;
        self.run_command("e2fsck-version", Command::new("e2fsck").arg("-V"))?;
        validate_runtime_directory(
            Path::new(&libublk::ctrl::UblkCtrl::run_dir()),
            fs::metadata(self.owned.path())?.uid(),
        )?;
        let control = fs::symlink_metadata("/dev/ublk-control").map_err(|error| io::Error::new(error.kind(), "AWAITING OPERATOR VALIDATION: /dev/ublk-control unavailable; operator must load ublk_drv and grant normal-user device access"))?;
        if !control.file_type().is_char_device() {
            return Err(io::Error::other("ublk-control is not a character device"));
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open("/dev/ublk-control")?;
        let helper = fs::symlink_metadata(MOUNT_HELPER).map_err(|error| io::Error::new(error.kind(), "AWAITING OPERATOR VALIDATION: install the root-owned mount helper and scoped sudo rule"))?;
        if !helper.is_file() || helper.uid() != 0 || helper.mode() & 0o022 != 0 {
            return Err(io::Error::other(
                "mount helper must be a root-owned regular file, not writable by group or others",
            ));
        }
        let built_helper = self
            .executable
            .parent()
            .expect("lab executable parent")
            .join("block-storage-mount");
        if fs::read(MOUNT_HELPER)? != fs::read(&built_helper)? {
            return Err(io::Error::other(
                "AWAITING OPERATOR VALIDATION: installed helper differs from the current build; operator must review and reinstall it",
            ));
        }
        self.run_command(
            "helper-preflight",
            Command::new("sudo").args(["-n", "--", MOUNT_HELPER, "check"]),
        )?;
        self.evidence
            .line(&format!("expected manifest:\n{}", workload::manifest()))
    }

    pub(super) fn backing_path(&self) -> PathBuf {
        self.owned.path().join("backing.img")
    }
    pub(super) fn mount_path(&self) -> PathBuf {
        self.owned.path().join("mount")
    }

    pub(super) fn validate_backing(&self) -> io::Result<()> {
        self.owned.validate()?;
        let metadata = fs::symlink_metadata(self.backing_path())?;
        if !metadata.is_file()
            || metadata.len() != BACKING_BYTES
            || metadata.uid() != fs::metadata(self.owned.path())?.uid()
            || Some(FileIdentity::from_metadata(&metadata)) != self.backing_identity
        {
            return Err(io::Error::other(
                "backing identity, owner, type, or size changed",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_device(&mut self) -> io::Result<&Device> {
        self.validate_backing()?;
        if self
            .daemon
            .as_mut()
            .ok_or_else(|| io::Error::other("daemon is absent"))?
            .try_wait()?
            .is_some()
        {
            return Err(io::Error::other("daemon exited unexpectedly"));
        }
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| io::Error::other("device identity is missing"))?;
        device.validate()?;
        Ok(device)
    }

    pub(super) fn require_unmounted(&mut self) -> io::Result<()> {
        let number = self.validate_device()?.number;
        fs_support::require_unmounted(&fs_support::mounts()?, number, &self.mount_path())?;
        let metadata = fs::symlink_metadata(self.mount_path())?;
        if !metadata.is_dir() || Some(FileIdentity::from_metadata(&metadata)) != self.mount_identity
        {
            return Err(io::Error::other(
                "unmounted mount-directory identity changed",
            ));
        }
        Ok(())
    }

    pub(super) fn require_mounted(&mut self) -> io::Result<()> {
        let number = self.validate_device()?.number;
        let id = fs_support::require_mounted(&fs_support::mounts()?, number, &self.mount_path())?;
        if self.mount_id.is_some_and(|expected| expected != id) {
            return Err(io::Error::other("mount identity changed"));
        }
        Ok(())
    }
}

fn validate_runtime_directory(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io::Error::new(error.kind(), format!("AWAITING OPERATOR VALIDATION: libublk runtime directory {} is missing; rerun sudo bash scripts/install-ext4-helper.sh", path.display())))?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(
            "libublk runtime directory must be a private caller-owned directory (mode 0700)",
        ));
    }
    Ok(())
}

pub(super) fn new_output(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

pub(super) fn filesystem_command(step: Step, device: &Path) -> Command {
    let mut command = match step {
        Step::Mkfs => Command::new("mkfs.ext4"),
        Step::Check(_) => {
            let mut command = Command::new("e2fsck");
            command.args(["-f", "-n"]);
            command
        }
        _ => unreachable!("only filesystem commands use this constructor"),
    };
    command.arg(device);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_lab(test: impl FnOnce(&mut Lab<'_>)) {
        let mut repo = OwnedTempDir::create(Path::new("/tmp")).unwrap();
        {
            let mut evidence = Evidence::create_ext4(repo.path()).unwrap();
            let now = Instant::now();
            let mut lab = Lab::new(
                &mut evidence,
                Duration::from_secs(2),
                now + Duration::from_secs(10),
                now,
            )
            .unwrap();
            test(&mut lab);
            assert!(lab.command.is_none() && lab.daemon.is_none());
            if lab.mount_path().exists() {
                fs::remove_dir(lab.mount_path()).unwrap();
            }
            lab.owned.cleanup().unwrap();
            lab.finished = true;
        }
        fs::remove_dir_all(repo.path().join("evidence")).unwrap();
        repo.cleanup().unwrap();
    }

    #[test]
    fn failed_and_timed_out_commands_preserve_output_and_reap_children() {
        with_lab(|lab| {
            let failure = lab.run_command(
                "failure",
                Command::new("sh").args([
                    "-c",
                    "printf failure-stdout; printf failure-stderr >&2; exit 7",
                ]),
            );
            assert!(failure.is_err());
            assert!(lab.command.is_none());
            let log = fs::read_to_string(lab.evidence.path()).unwrap();
            assert!(
                log.contains("failure-stdout")
                    && log.contains("failure-stderr")
                    && log.contains("outcome=Ok(Exit")
            );
            lab.per_command = Duration::from_millis(100);
            let timeout = lab.run_command(
                "timeout",
                Command::new("sh").args(["-c", "printf before-timeout; sleep 60"]),
            );
            assert_eq!(timeout.unwrap_err().kind(), io::ErrorKind::TimedOut);
            assert!(lab.command.is_none());
            assert!(
                fs::read_to_string(lab.evidence.path())
                    .unwrap()
                    .contains("before-timeout")
            );
        });
    }

    #[test]
    fn suite_deadline_prevents_spawning_another_command() {
        with_lab(|lab| {
            lab.suite_deadline = Instant::now();
            let error = lab
                .run_command("must-not-start", &mut Command::new("true"))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_eq!(lab.sequence, 0);
            assert!(lab.command.is_none());
        });
    }

    #[test]
    fn cleanup_refuses_unexpected_entries_before_removing_backing() {
        with_lab(|lab| {
            fs::create_dir(lab.mount_path()).unwrap();
            lab.mount_identity = Some(FileIdentity::from_metadata(
                &fs::metadata(lab.mount_path()).unwrap(),
            ));
            let file = new_output(&lab.backing_path()).unwrap();
            file.set_len(BACKING_BYTES).unwrap();
            lab.backing_identity = Some(FileIdentity::from_metadata(&file.metadata().unwrap()));
            let replacement = lab.owned.path().join("unexpected");
            std::os::unix::fs::symlink("/nonexistent", &replacement).unwrap();
            assert!(lab.finish().is_err());
            assert_eq!(
                fs::metadata(lab.backing_path()).unwrap().len(),
                BACKING_BYTES
            );
            fs::remove_file(replacement).unwrap();
            lab.finish().unwrap();
            assert!(!lab.owned.path().exists());
        });
    }

    #[test]
    fn runtime_preflight_requires_an_existing_private_owned_directory() {
        let mut owned = OwnedTempDir::create(Path::new("/tmp")).unwrap();
        let uid = fs::metadata(owned.path()).unwrap().uid();
        validate_runtime_directory(owned.path(), uid).unwrap();
        assert!(validate_runtime_directory(owned.path(), uid + 1).is_err());
        let missing = owned.path().join("missing");
        assert!(validate_runtime_directory(&missing, uid).is_err());
        std::os::unix::fs::symlink(owned.path(), &missing).unwrap();
        assert!(validate_runtime_directory(&missing, uid).is_err());
        fs::remove_file(&missing).unwrap();
        fs::set_permissions(owned.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_runtime_directory(owned.path(), uid).is_err());
        fs::set_permissions(owned.path(), fs::Permissions::from_mode(0o700)).unwrap();
        owned.cleanup().unwrap();
    }

    #[test]
    fn filesystem_commands_use_defaults_and_read_only_checks() {
        let path = Path::new("/dev/ublkb17");
        let mkfs = filesystem_command(Step::Mkfs, path);
        assert_eq!(mkfs.get_program(), "mkfs.ext4");
        assert_eq!(mkfs.get_args().collect::<Vec<_>>(), [path.as_os_str()]);
        let check = filesystem_command(Step::Check(1), path);
        assert_eq!(check.get_program(), "e2fsck");
        assert_eq!(
            check.get_args().collect::<Vec<_>>(),
            ["-f", "-n", "/dev/ublkb17"]
        );
    }
}
