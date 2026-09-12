use super::*;
use commands::{filesystem_command, new_output};

impl Lab<'_> {
    pub(super) fn lifecycle(&mut self) -> io::Result<()> {
        for step in STEPS {
            self.deadline()?;
            self.evidence.line(&format!("step: {step:?}"))?;
            match step {
                Step::Format => self.format()?,
                Step::Start => self.start()?,
                Step::Mkfs | Step::Check(_) => {
                    self.require_unmounted()?;
                    let path = self
                        .device
                        .as_ref()
                        .expect("validated device")
                        .path
                        .as_path()
                        .to_owned();
                    self.run_command(&format!("{step:?}"), &mut filesystem_command(step, &path))?;
                }
                Step::Mount => self.mount()?,
                Step::Populate | Step::Verify => self.files(step == Step::Populate)?,
                Step::Unmount => self.unmount()?,
                Step::Stop => self.stop()?,
            }
            self.check_daemon_errors()?;
        }
        Ok(())
    }

    fn format(&mut self) -> io::Result<()> {
        self.owned.validate()?;
        fs::create_dir(self.mount_path())?;
        self.mount_identity = Some(FileIdentity::from_metadata(&fs::symlink_metadata(
            self.mount_path(),
        )?));
        let mut command = Command::new(&self.daemon_executable);
        command
            .arg("format")
            .arg(self.backing_path())
            .arg(BACKING_BYTES.to_string())
            .arg(VOLUME_BYTES.to_string());
        self.run_command("format", &mut command)?;
        self.backing_identity = Some(FileIdentity::from_metadata(&fs::symlink_metadata(
            self.backing_path(),
        )?));
        self.validate_backing()
    }

    fn start(&mut self) -> io::Result<()> {
        if self.daemon.is_some() || self.device.is_some() {
            return Err(io::Error::other("previous daemon or device remains"));
        }
        self.validate_backing()?;
        let deadline = self.deadline()?;
        let (stdout, stderr) = self.outputs("daemon");
        let mut command = Command::new(&self.daemon_executable);
        command
            .arg("serve")
            .arg(self.backing_path())
            .arg("-1")
            .stdin(Stdio::null())
            .env_remove("BLOCK_STORAGE_TEST_FAILPOINT")
            .stdout(Stdio::from(new_output(&stdout)?))
            .stderr(Stdio::from(new_output(&stderr)?));
        self.log_command("serve", &command)?;
        self.daemon = Some(ManagedChild::spawn(&mut command)?);
        self.daemon_outputs = Some((stdout.clone(), stderr));
        self.evidence.line(&format!(
            "daemon pid={}",
            self.daemon.as_ref().expect("spawned daemon").pid()
        ))?;
        loop {
            if self
                .daemon
                .as_mut()
                .expect("spawned daemon")
                .try_wait()?
                .is_some()
            {
                return Err(io::Error::other("daemon exited before readiness"));
            }
            let text = fs::read_to_string(&stdout)?;
            if text.ends_with('\n') {
                let path = readiness_path(&text)?;
                match Device::read(path) {
                    Ok(device) => {
                        self.device = Some(device);
                        break;
                    }
                    Err(error) if Instant::now() >= deadline => return Err(error),
                    Err(_) => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon readiness timed out",
                ));
            }
            thread::sleep(POLL);
        }
        self.require_unmounted()?;
        let device = self.device.as_ref().expect("ready device");
        self.evidence.line(&format!(
            "device: {} number={:?} class={} node={}",
            device.path.as_path().display(),
            device.number,
            device.class_identity,
            device.node_identity
        ))?;
        let sys = Path::new("/sys/block").join(format!("ublkb{}", device.path.id()));
        for (field, expected) in [
            ("queue/logical_block_size", "4096"),
            ("queue/physical_block_size", "4096"),
            ("queue/max_sectors_kb", "4"),
        ] {
            let actual = fs::read_to_string(sys.join(field))?;
            self.evidence.line(&format!("{field}: {}", actual.trim()))?;
            if actual.trim() != expected {
                return Err(io::Error::other(format!("unexpected {field}: {actual}")));
            }
        }
        Ok(())
    }

    fn helper(&mut self, action: &str) -> io::Result<()> {
        let device = self.validate_device()?;
        let id = device.path.id().to_string();
        let name = self
            .owned
            .path()
            .file_name()
            .expect("owned directory name")
            .to_owned();
        self.run_command(
            action,
            Command::new("sudo")
                .args(["-n", "--", MOUNT_HELPER, action, &id])
                .arg(name),
        )?;
        Ok(())
    }

    fn mount(&mut self) -> io::Result<()> {
        self.require_unmounted()?;
        self.mount_attempted = true;
        self.helper("mount")?;
        self.require_mounted()?;
        let number = self.device.as_ref().expect("validated device").number;
        self.mount_id = Some(fs_support::require_mounted(
            &fs_support::mounts()?,
            number,
            &self.mount_path(),
        )?);
        self.evidence.line(&format!(
            "mount id={:?} target={} options=ext4-defaults,nosuid,nodev",
            self.mount_id,
            self.mount_path().display()
        ))
    }

    fn files(&mut self, populate: bool) -> io::Result<()> {
        self.require_mounted()?;
        let mut command = Command::new(&self.executable);
        command
            .arg(if populate {
                "ext4-populate"
            } else {
                "ext4-verify"
            })
            .current_dir(self.mount_path().join("work"));
        let actual = self.run_command("workload", &mut command)?;
        if actual != workload::manifest() {
            return Err(io::Error::other(
                "workload manifest differs from independent expected result",
            ));
        }
        Ok(())
    }

    fn unmount(&mut self) -> io::Result<()> {
        if self.command.is_some() {
            return Err(io::Error::other(
                "workload children must be reaped before unmount",
            ));
        }
        self.require_mounted()?;
        self.helper("unmount")?;
        self.require_unmounted()?;
        self.mount_id = None;
        self.mount_attempted = false;
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        self.require_unmounted()?;
        let id = self
            .device
            .as_ref()
            .expect("validated device")
            .path
            .id()
            .to_string();
        let stats = fs::read_to_string(format!("/sys/block/ublkb{id}/stat"))?;
        self.evidence.line(&format!(
            "kernel block I/O counters before stop: {}",
            stats.trim()
        ))?;
        let deadline = self.deadline()?;
        let child = self.daemon.as_mut().expect("validated daemon");
        child.terminate()?;
        let outcome = child.wait_bounded(deadline)?;
        if !matches!(outcome, Outcome::Exit(ref status) if status.success()) {
            return Err(io::Error::other(format!(
                "clean daemon stop failed: {outcome:?}"
            )));
        }
        child.cleanup()?;
        self.daemon = None;
        self.evidence
            .line(&format!("daemon SIGTERM: {outcome:?}"))?;
        for path in [
            format!("/dev/ublkb{id}"),
            format!("/sys/block/ublkb{id}"),
            format!("/sys/class/ublk-char/ublkc{id}"),
        ] {
            while fs::symlink_metadata(&path).is_ok() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("device remains after clean stop: {path}"),
                    ));
                }
                thread::sleep(POLL);
            }
            if let Err(error) = fs::symlink_metadata(&path)
                && error.kind() != io::ErrorKind::NotFound
            {
                return Err(error);
            }
        }
        self.device = None;
        self.log_daemon()
    }

    fn check_daemon_errors(&self) -> io::Result<()> {
        if let Some((_, stderr)) = &self.daemon_outputs {
            let text = fs::read_to_string(stderr)?;
            if text.contains("request rejected:") || text.contains("panicked") {
                return Err(io::Error::other(
                    "daemon rejected a request or panicked; inspect preserved daemon stderr",
                ));
            }
        }
        Ok(())
    }

    fn log_daemon(&mut self) -> io::Result<()> {
        if let Some((stdout, stderr)) = self.daemon_outputs.clone() {
            self.capture("daemon stdout", &stdout)?;
            self.capture("daemon stderr", &stderr)?;
            self.check_daemon_errors()?;
        }
        self.daemon_outputs = None;
        Ok(())
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        if self.daemon.is_some()
            || self.device.is_some()
            || self.command.is_some()
            || self.mount_attempted
        {
            return Err(io::Error::other("owned live resources remain"));
        }
        self.validate_backing()?;
        if fs_support::mounts()?
            .iter()
            .any(|mount| mount.target.starts_with(self.owned.path()))
        {
            return Err(io::Error::other("mount remains under owned fixture"));
        }
        let metadata = fs::symlink_metadata(self.mount_path())?;
        if Some(FileIdentity::from_metadata(&metadata)) != self.mount_identity {
            return Err(io::Error::other("mount directory changed before removal"));
        }
        // Validate the whole directory before cleanup can unlink the backing image.
        for entry in fs::read_dir(self.owned.path())? {
            let entry = entry?;
            if entry.path() == self.mount_path() {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.is_file()
                || metadata.uid() != fs::metadata(self.owned.path())?.uid()
                || metadata.dev() != fs::metadata(self.owned.path())?.dev()
            {
                return Err(io::Error::other(
                    "unexpected cleanup entry; preserving the image and evidence",
                ));
            }
        }
        fs::remove_dir(self.mount_path())?;
        self.owned.cleanup()?;
        if self.owned.path().try_exists()? {
            return Err(io::Error::other(
                "temporary directory remains after cleanup",
            ));
        }
        self.finished = true;
        Ok(())
    }

    pub(super) fn recover_error(&mut self) -> io::Result<()> {
        // A single cleanup reserve bounds safe unmount/stop after the workload deadline expires.
        self.suite_deadline = Instant::now() + Duration::from_secs(60);
        self.evidence.line("failure cleanup: at most 60s plus bounded child reaping; backing and logs are retained")?;
        if let Some(child) = self.command.as_mut() {
            child.cleanup()?;
            self.command = None;
        }
        if self.device.is_none()
            && let Some(child) = self.daemon.as_mut()
        {
            if child.try_wait()?.is_none() {
                return Err(io::Error::other(
                    "daemon identity was not established; preserving process and image",
                ));
            }
            child.cleanup()?;
            self.daemon = None;
            return self.log_daemon();
        }
        if self.device.is_some() {
            let number = self.validate_device()?.number;
            let mounts = fs_support::mounts()?;
            if fs_support::require_unmounted(&mounts, number, &self.mount_path()).is_err() {
                if !self.mount_attempted {
                    return Err(io::Error::other(
                        "unrecorded mount appeared; preserving daemon",
                    ));
                }
                self.unmount()?;
            }
            self.stop()?;
        }
        self.log_daemon()
    }
}

fn readiness_path(text: &str) -> io::Result<DevicePath> {
    if !text.ends_with('\n') || text.lines().count() != 1 {
        return Err(io::Error::other(
            "readiness must be exactly one flushed device path",
        ));
    }
    text.trim_end_matches('\n')
        .parse()
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_never_accepts_arbitrary_devices_or_extra_output() {
        assert_eq!(readiness_path("/dev/ublkb7\n").unwrap().id().get(), 7);
        for text in [
            "/dev/sda\n",
            "/dev/ublkb7",
            "/dev/ublkb7\nextra\n",
            "/dev/ublkb7\n\n",
            "/dev/ublkb../sda\n",
        ] {
            assert!(readiness_path(text).is_err());
        }
    }
}
