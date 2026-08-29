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
        lifecycle::validate_backing(&self.path, owned, self.bytes, Some(self.identity)).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceIdentityState {
    Matching,
    Absent,
    IdentityChanged,
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

    fn still_matches(&self, paths: &Paths) -> io::Result<DeviceIdentityState> {
        self.classify_identity(
            paths,
            ublk::validate_device_identity(&paths.dev_root, &paths.sys_root, &self.path),
        )
    }

    fn classify_identity(
        &self,
        paths: &Paths,
        validation: io::Result<FileIdentity>,
    ) -> io::Result<DeviceIdentityState> {
        match validation {
            Ok(identity) if identity == self.class_identity => Ok(DeviceIdentityState::Matching),
            Ok(_) => Ok(DeviceIdentityState::IdentityChanged),
            Err(validation_error) => match daemon::device_is_absent(paths, self.id()) {
                Ok(true) => Ok(DeviceIdentityState::Absent),
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

mod daemon;
mod lifecycle;

pub use lifecycle::run;

#[cfg(test)]
mod tests;
