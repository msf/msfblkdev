use crate::evidence::Evidence;
use crate::process::{ManagedChild, Outcome};
#[cfg(test)]
use crate::ublk::RECORD_CAPACITY;
use crate::ublk::{
    self, BLOCK_BYTES, DevicePath, FileIdentity, FioOptions, FioRw, Geometry, OwnedTempDir,
};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const LOCK_ERROR: &str = "block-storage-ublk: backing file is already locked\n";

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
    StoppedMatching,
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
            Err(validation_error) => self.classify_inactive_identity(paths).map_err(|state_error| {
                io::Error::other(format!(
                    "cannot prove recorded device identity: {validation_error}; state check failed: {state_error}"
                ))
            }),
        }
    }

    fn classify_inactive_identity(&self, paths: &Paths) -> io::Result<DeviceIdentityState> {
        let id = self.id();
        let dev_present = path_is_present(&paths.dev_root.join(format!("ublkb{id}")))?;
        let block_present = path_is_present(&paths.sys_root.join(format!("block/ublkb{id}")))?;
        let class_path = paths.sys_root.join(format!("class/ublk-char/ublkc{id}"));
        let class_present = path_is_present(&class_path)?;

        match (dev_present, block_present, class_present) {
            (false, false, false) => Ok(DeviceIdentityState::Absent),
            (false, false, true) => {
                let metadata = fs::metadata(class_path)?;
                if !metadata.is_dir() {
                    return Err(io::Error::other(
                        "remaining ublk sysfs class entry is not a directory",
                    ));
                }
                if FileIdentity::from_metadata(&metadata) == self.class_identity {
                    Ok(DeviceIdentityState::StoppedMatching)
                } else {
                    Ok(DeviceIdentityState::IdentityChanged)
                }
            }
            _ => Err(io::Error::other(
                "recorded device paths are only partially present",
            )),
        }
    }
}

fn path_is_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
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
    preserve_for_identity: bool,
}

mod daemon;
mod lifecycle;
mod scenario;
mod scenario_run;

pub use lifecycle::run;

#[cfg(test)]
mod tests;
