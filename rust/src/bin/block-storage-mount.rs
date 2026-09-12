#[path = "fs_support/mod.rs"]
mod fs_support;

use fs_support::{BACKING_BYTES, MOUNT_HELPER, VOLUME_BYTES};
use libublk::ctrl::UblkCtrl;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, PartialEq)]
enum Action {
    Check,
    Mount(u32, String),
    Unmount(u32, String),
}

fn parse_args(args: &[String]) -> io::Result<Action> {
    if args == ["check"] {
        return Ok(Action::Check);
    }
    let [action, id, name] = args else {
        return Err(usage());
    };
    if !fs_support::owned_name(name) || id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(usage());
    }
    let id: u32 = id.parse().map_err(|_| usage())?;
    if id > i32::MAX as u32 {
        return Err(usage());
    }
    match action.as_str() {
        "mount" => Ok(Action::Mount(id, name.clone())),
        "unmount" => Ok(Action::Unmount(id, name.clone())),
        _ => Err(usage()),
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "usage: {MOUNT_HELPER} check | mount <ublk-id> <owned-directory-name> | unmount <ublk-id> <owned-directory-name>"
        ),
    )
}

fn invoking_user() -> io::Result<(u32, u32)> {
    // SAFETY: geteuid has no arguments or memory-safety preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "run the installed root-owned helper through sudo",
        ));
    }
    let parse = |key| {
        std::env::var(key)
            .map_err(io::Error::other)?
            .parse::<u32>()
            .map_err(io::Error::other)
    };
    let uid = parse("SUDO_UID")?;
    let gid = parse("SUDO_GID")?;
    if uid == 0 {
        return Err(io::Error::other("helper requires a non-root sudo caller"));
    }
    Ok((uid, gid))
}

fn directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

struct Fixture {
    parent: File,
    target: File,
    device: File,
    path: PathBuf,
    number: (u32, u32),
}

impl Fixture {
    fn open(id: u32, name: &str, uid: u32) -> io::Result<Self> {
        let path = Path::new("/tmp").join(name).join("mount");
        let parent = directory(path.parent().ok_or_else(usage)?)?;
        let metadata = parent.metadata()?;
        if metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
            return Err(io::Error::other(
                "fixture parent must be caller-owned mode 0700",
            ));
        }
        let backing = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(fd_path(&parent).join("backing.img"))?;
        let metadata = backing.metadata()?;
        if !metadata.is_file() || metadata.uid() != uid || metadata.len() != BACKING_BYTES {
            return Err(io::Error::other(
                "fixture backing is not the caller-owned bounded regular file",
            ));
        }
        let device = verified_device(id, uid)?;
        let metadata = device.metadata()?;
        let number = (libc::major(metadata.rdev()), libc::minor(metadata.rdev()));
        let target = directory(&fd_path(&parent).join("mount"))?;
        Ok(Self {
            parent,
            target,
            device,
            path,
            number,
        })
    }

    fn mount(&self, uid: u32, gid: u32) -> io::Result<()> {
        fs_support::require_unmounted(&fs_support::mounts()?, self.number, &self.path)?;
        let metadata = self.target.metadata()?;
        if metadata.uid() != uid
            || metadata.dev() != self.parent.metadata()?.dev()
            || fs::read_dir(fd_path(&self.target))?.next().is_some()
        {
            return Err(io::Error::other(
                "mount directory is not empty and caller-owned on the fixture filesystem",
            ));
        }
        let source = CString::new(fd_path(&self.device).as_os_str().as_encoded_bytes())
            .map_err(io::Error::other)?;
        let target = CString::new(fd_path(&self.target).as_os_str().as_encoded_bytes())
            .map_err(io::Error::other)?;
        // SAFETY: all strings are live NUL-terminated strings. Source and target are pinned descriptors.
        // nosuid/nodev restrict the privilege grant; ext4 journaling and barrier defaults are unchanged.
        let result = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                c"ext4".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                std::ptr::null(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let result = fs_support::require_mounted(&fs_support::mounts()?, self.number, &self.path)
            .and_then(|_| self.create_work_directory(uid, gid));
        if let Err(error) = result {
            // A moved or substituted mount is preserved, not forcibly detached.
            let rollback = self.rollback_mount();
            return Err(io::Error::other(format!(
                "mount setup failed: {error}; rollback: {rollback:?}; fixture={}",
                self.path.display()
            )));
        }
        Ok(())
    }

    fn rollback_mount(&self) -> io::Result<()> {
        let mounted = Self {
            parent: self.parent.try_clone()?,
            target: directory(&fd_path(&self.parent).join("mount"))?,
            device: self.device.try_clone()?,
            path: self.path.clone(),
            number: self.number,
        };
        mounted.unmount()
    }

    fn create_work_directory(&self, uid: u32, gid: u32) -> io::Result<()> {
        let root = directory(&fd_path(&self.parent).join("mount"))?;
        let root_metadata = root.metadata()?;
        if root_metadata.dev() != self.device.metadata()?.rdev()
            || root_metadata.uid() != 0
            || root_metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::other(
                "mounted root must belong to the recorded device and be root-owned without group/other write permission",
            ));
        }
        let path = fd_path(&root).join("work");
        // A protected parent and mode 0700 prevent replacement or a user mount before fchown.
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                let work = directory(&path)?;
                let metadata = work.metadata()?;
                if metadata.uid() != 0
                    || metadata.dev() != root_metadata.dev()
                    || metadata.mode() & 0o077 != 0
                {
                    return Err(io::Error::other(
                        "new work directory identity or permissions changed",
                    ));
                }
                // SAFETY: fchown targets only the protected, newly created directory descriptor.
                if unsafe { libc::fchown(work.as_raw_fd(), uid, gid) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                work.sync_all()?;
                root.sync_all()
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = directory(&path)?.metadata()?;
                if metadata.uid() != uid
                    || metadata.gid() != gid
                    || metadata.dev() != root.metadata()?.dev()
                {
                    return Err(io::Error::other(
                        "existing work directory has unexpected ownership or filesystem",
                    ));
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn unmount(self) -> io::Result<()> {
        let id = fs_support::require_mounted(&fs_support::mounts()?, self.number, &self.path)?;
        let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", self.target.as_raw_fd()))?;
        if !fdinfo.lines().any(|line| {
            line.strip_prefix("mnt_id:")
                .is_some_and(|value| value.trim() == id.to_string())
        }) {
            return Err(io::Error::other(
                "opened mount descriptor differs from mountinfo identity",
            ));
        }
        let target = CString::new(
            fd_path(&self.parent)
                .join("mount")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .map_err(io::Error::other)?;
        // An open directory on the mounted filesystem would make unmount return EBUSY.
        drop(self.target);
        // SAFETY: the parent is pinned; UMOUNT_NOFOLLOW refuses a substituted final symlink.
        // No force or lazy unmount is permitted.
        if unsafe { libc::umount2(target.as_ptr(), libc::UMOUNT_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        fs_support::require_unmounted(&fs_support::mounts()?, self.number, &self.path)
    }
}

fn helper_lock() -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/run/block-storage-mount.lock")?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::other(
            "helper lock must be a private root-owned regular file",
        ));
    }
    // SAFETY: flock applies only to this open descriptor. Contention fails without waiting.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn verified_device(id: u32, uid: u32) -> io::Result<File> {
    let class_path = PathBuf::from(format!("/sys/class/ublk-char/ublkc{id}"));
    let class = directory(&class_path.canonicalize()?)?;
    let original = class.metadata()?;
    let control = UblkCtrl::new_simple(i32::try_from(id).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    require_owner(&control, uid)?;
    let device = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("/dev/ublkb{id}"))?;
    let metadata = device.metadata()?;
    let number = (libc::major(metadata.rdev()), libc::minor(metadata.rdev()));
    let sys_number = fs::read_to_string(format!("/sys/block/ublkb{id}/dev"))?;
    if !metadata.file_type().is_block_device()
        || sys_number.trim() != format!("{}:{}", number.0, number.1)
    {
        return Err(io::Error::other(
            "device node does not match kernel device identity",
        ));
    }
    control.read_dev_info().map_err(io::Error::other)?;
    require_owner(&control, uid)?;
    let current = fs::metadata(&class_path)?;
    if current.dev() != original.dev() || current.ino() != original.ino() {
        return Err(io::Error::other(
            "ublk identity changed while opening device",
        ));
    }
    Ok(device)
}

fn require_owner(control: &UblkCtrl, uid: u32) -> io::Result<()> {
    let info = control.dev_info();
    let mut params = libublk::sys::ublk_params::default();
    control.get_params(&mut params).map_err(io::Error::other)?;
    validate_device_parameters(info.owner_uid, info.flags, &params, uid)
}

fn validate_device_parameters(
    owner: u32,
    flags: u64,
    params: &libublk::sys::ublk_params,
    uid: u32,
) -> io::Result<()> {
    if owner != uid || flags & u64::from(libublk::sys::UBLK_F_UNPRIVILEGED_DEV) == 0 {
        return Err(io::Error::other(
            "ublk device is not an unprivileged device owned by the sudo caller",
        ));
    }
    if params.basic.logical_bs_shift != 12 || params.basic.dev_sectors != VOLUME_BYTES / 512 {
        return Err(io::Error::other(
            "ublk geometry differs from the ext4 fixture",
        ));
    }
    Ok(())
}

fn execute() -> io::Result<()> {
    let action = parse_args(&std::env::args().skip(1).collect::<Vec<_>>())?;
    let (uid, gid) = invoking_user()?;
    std::env::set_current_dir("/")?;
    // SAFETY: install the default termination disposition in this single-purpose process.
    if unsafe { libc::signal(libc::SIGALRM, libc::SIG_DFL) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: initialize a signal set and unblock only SIGALRM in this single-threaded process.
    unsafe {
        let mut signals = std::mem::zeroed();
        if libc::sigemptyset(&mut signals) != 0
            || libc::sigaddset(&mut signals, libc::SIGALRM) != 0
            || libc::sigprocmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut()) != 0
        {
            return Err(io::Error::last_os_error());
        }
        libc::alarm(60);
    }
    let _lock = helper_lock()?;
    match action {
        Action::Check => println!("block-storage-mount protocol=1 uid={uid}"),
        Action::Mount(id, name) => Fixture::open(id, &name, uid)?.mount(uid, gid)?,
        Action::Unmount(id, name) => Fixture::open(id, &name, uid)?.unmount()?,
    }
    Ok(())
}

fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("block-storage-mount: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_requires_kernel_ownership_unprivileged_flag_and_exact_geometry() {
        let flags = u64::from(libublk::sys::UBLK_F_UNPRIVILEGED_DEV);
        let mut params = libublk::sys::ublk_params::default();
        params.basic.logical_bs_shift = 12;
        params.basic.dev_sectors = VOLUME_BYTES / 512;
        validate_device_parameters(1000, flags, &params, 1000).unwrap();
        assert!(validate_device_parameters(1001, flags, &params, 1000).is_err());
        assert!(validate_device_parameters(1000, 0, &params, 1000).is_err());
        params.basic.logical_bs_shift = 9;
        assert!(validate_device_parameters(1000, flags, &params, 1000).is_err());
        params.basic.logical_bs_shift = 12;
        params.basic.dev_sectors += 8;
        assert!(validate_device_parameters(1000, flags, &params, 1000).is_err());
    }

    #[test]
    fn helper_rejects_paths_options_and_arbitrary_device_names() {
        assert_eq!(parse_args(&["check".into()]).unwrap(), Action::Check);
        assert_eq!(
            parse_args(&[
                "mount".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::Mount(7, "my-block-storage-ublk.12.0".into())
        );
        for args in [
            vec!["mount", "/dev/sda", "my-block-storage-ublk.12.0"],
            vec!["mount", "7", "/tmp/anything"],
            vec!["unmount", "7", "../../etc"],
            vec!["mount", "7", "my-block-storage-ublk.12.0", "-o", "suid"],
            vec!["check", "extra"],
        ] {
            assert!(parse_args(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
    }
}
