mod commands;
mod lifecycle;
pub mod workload;

use crate::evidence::Evidence;
use crate::fs_support::{self, BACKING_BYTES, MOUNT_HELPER, VOLUME_BYTES};
use crate::process::{ManagedChild, Outcome};
use crate::ublk::{self, DevicePath, FileIdentity, OwnedTempDir};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// Acceptance repeats the complete lifecycle, never reusing a previous repetition's log.
const REPETITIONS: usize = 3;
const POLL: Duration = Duration::from_millis(10);
const MAX_CAPTURE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Format,
    Start,
    Mkfs,
    Check(u8),
    Mount,
    Populate,
    Verify,
    Unmount,
    Stop,
}

const STEPS: [Step; 17] = [
    Step::Format,
    Step::Start,
    Step::Mkfs,
    Step::Check(1),
    Step::Mount,
    Step::Populate,
    Step::Verify,
    Step::Unmount,
    Step::Check(2),
    Step::Stop,
    Step::Start,
    Step::Check(3),
    Step::Mount,
    Step::Verify,
    Step::Unmount,
    Step::Check(4),
    Step::Stop,
];

struct Device {
    path: DevicePath,
    class_identity: FileIdentity,
    node_identity: FileIdentity,
    number: (u32, u32),
}

impl Device {
    fn read(path: DevicePath) -> io::Result<Self> {
        let class_identity = ublk::validate_device_identity_for(
            Path::new("/dev"),
            Path::new("/sys"),
            &path,
            VOLUME_BYTES,
        )?;
        let metadata = fs::symlink_metadata(path.as_path())?;
        if !metadata.file_type().is_block_device() {
            return Err(io::Error::other("device path is not a block node"));
        }
        // Readiness can precede udev's ownership handoff, as in the existing fio lab.
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path.as_path())?;
        if FileIdentity::from_metadata(&opened.metadata()?)
            != FileIdentity::from_metadata(&metadata)
        {
            return Err(io::Error::other("device node changed while opening it"));
        }
        Ok(Self {
            path,
            class_identity,
            node_identity: FileIdentity::from_metadata(&metadata),
            number: (libc::major(metadata.rdev()), libc::minor(metadata.rdev())),
        })
    }

    fn validate(&self) -> io::Result<()> {
        let actual = Self::read(self.path.clone())?;
        if actual.class_identity != self.class_identity
            || actual.node_identity != self.node_identity
            || actual.number != self.number
        {
            return Err(io::Error::other("recorded ublk identity changed"));
        }
        Ok(())
    }
}

struct Lab<'a> {
    owned: OwnedTempDir,
    mount_identity: Option<FileIdentity>,
    backing_identity: Option<FileIdentity>,
    device: Option<Device>,
    daemon: Option<ManagedChild>,
    command: Option<ManagedChild>,
    daemon_outputs: Option<(PathBuf, PathBuf)>,
    mount_id: Option<u64>,
    mount_attempted: bool,
    executable: PathBuf,
    daemon_executable: PathBuf,
    evidence: &'a mut Evidence,
    sequence: usize,
    per_command: Duration,
    suite_deadline: Instant,
    started: Instant,
    finished: bool,
}

impl Drop for Lab<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.owned.preserve();
            eprintln!("preserved fixture: {}", self.owned.path().display());
            // Killing the device under an ambiguous mount would introduce an unrequested crash.
            for child in [&mut self.command, &mut self.daemon].into_iter().flatten() {
                eprintln!("preserved child pid={}", child.pid());
                child.preserve();
            }
        }
    }
}

pub fn preflight(repo: &Path, per_command: Duration, suite: Duration) -> io::Result<()> {
    let started = Instant::now();
    let mut evidence = Evidence::create_ext4(repo)?;
    println!("evidence: {}", evidence.path().display());
    let mut lab = Lab::new(&mut evidence, per_command, started + suite, started)?;
    if let Err(error) = lab.preflight(repo) {
        lab.evidence.line(&format!(
            "preflight: FAIL {error}; no device created or mounted"
        ))?;
        return Err(error);
    }
    lab.owned.cleanup()?;
    lab.finished = true;
    lab.evidence
        .line("preflight: PASS; no device created or mounted")
}

pub fn run(repo: &Path, per_command: Duration, suite: Duration) -> io::Result<()> {
    let started = Instant::now();
    let suite_deadline = started + suite;
    let mut evidence = Evidence::create_ext4(repo)?;
    println!("evidence: {}", evidence.path().display());
    evidence.line(&format!("limits: repetitions={REPETITIONS} per_command_s={} suite_s={} volume_bytes={VOLUME_BYTES} backing_bytes={BACKING_BYTES}", per_command.as_secs(), suite.as_secs()))?;
    for repetition in 1..=REPETITIONS {
        evidence.line(&format!("repetition: {repetition}/{REPETITIONS}"))?;
        let mut lab = Lab::new(&mut evidence, per_command, suite_deadline, started)?;
        let result = lab
            .preflight(repo)
            .and_then(|()| lab.lifecycle())
            .and_then(|()| lab.finish());
        if let Err(error) = result {
            lab.evidence.line(&format!("result: FAIL {error}"))?;
            if let Err(cleanup) = lab.recover_error() {
                lab.evidence.line(&format!("cleanup: {cleanup}"))?;
            }
            lab.evidence.line(&format!(
                "preserved fixture: {}",
                lab.owned.path().display()
            ))?;
            return Err(error);
        }
        lab.evidence.line(&format!(
            "repetition {repetition}: PASS; all owned resources removed"
        ))?;
    }
    evidence.line(&format!("elapsed_ms: {}", started.elapsed().as_millis()))?;
    evidence.line("result: PASS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_plan_has_four_unmounted_checks_and_clean_restart() {
        assert_eq!(REPETITIONS, 3);
        assert_eq!(
            STEPS
                .iter()
                .filter(|step| matches!(step, Step::Format))
                .count(),
            1
        );
        let mut mounted = false;
        let mut started = false;
        let mut checks = Vec::new();
        let mut starts = 0;
        let mut verifications = 0;
        for step in STEPS {
            match step {
                Step::Start => {
                    assert!(!started && !mounted);
                    started = true;
                    starts += 1;
                }
                Step::Stop => {
                    assert!(started && !mounted);
                    started = false;
                }
                Step::Mount => {
                    assert!(started && !mounted);
                    mounted = true;
                }
                Step::Unmount => {
                    assert!(started && mounted);
                    mounted = false;
                }
                Step::Check(number) => {
                    assert!(started && !mounted);
                    checks.push(number);
                }
                Step::Verify => {
                    assert!(mounted);
                    verifications += 1;
                }
                Step::Populate => assert!(mounted),
                Step::Format => assert!(!started && !mounted),
                Step::Mkfs => assert!(started && !mounted),
            }
        }
        assert_eq!(checks, [1, 2, 3, 4]);
        assert_eq!(starts, 2);
        assert_eq!(verifications, 2);
        assert!(!started && !mounted);
    }
}
