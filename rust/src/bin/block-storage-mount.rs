#[path = "fs_support/mod.rs"]
mod fs_support;

use fs_support::{BACKING_BYTES, Filesystem, MOUNT_HELPER, VOLUME_BYTES};
use libublk::ctrl::UblkCtrl;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const XFS_FORMATTER: &str = "/usr/sbin/mkfs.xfs";

#[derive(Debug, PartialEq)]
enum Action {
    Check,
    FormatXfs(u32, String),
    Mount(Filesystem, u32, String),
    Unmount(Filesystem, u32, String),
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
        "format-xfs" => Ok(Action::FormatXfs(id, name.clone())),
        "mount" => Ok(Action::Mount(Filesystem::Ext4, id, name.clone())),
        "unmount" => Ok(Action::Unmount(Filesystem::Ext4, id, name.clone())),
        "mount-xfs" => Ok(Action::Mount(Filesystem::Xfs, id, name.clone())),
        "unmount-xfs" => Ok(Action::Unmount(Filesystem::Xfs, id, name.clone())),
        _ => Err(usage()),
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "usage: {MOUNT_HELPER} check | format-xfs|mount|unmount|mount-xfs|unmount-xfs <ublk-id> <owned-directory-name>"
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
    filesystem: Filesystem,
    parent: File,
    target: File,
    device: File,
    path: PathBuf,
    number: (u32, u32),
}

impl Fixture {
    fn open(id: u32, name: &str, uid: u32, filesystem: Filesystem) -> io::Result<Self> {
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
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.len() != filesystem.backing_bytes()
        {
            return Err(io::Error::other(
                "fixture backing is not the caller-owned bounded regular file",
            ));
        }
        let device = verified_device(id, uid, filesystem)?;
        let metadata = device.metadata()?;
        let number = (libc::major(metadata.rdev()), libc::minor(metadata.rdev()));
        let target = directory(&fd_path(&parent).join("mount"))?;
        Ok(Self {
            filesystem,
            parent,
            target,
            device,
            path,
            number,
        })
    }

    fn format_xfs(&self, lock: &File) -> io::Result<()> {
        if self.filesystem != Filesystem::Xfs {
            return Err(io::Error::other(
                "privileged formatting requires an XFS fixture",
            ));
        }
        fs_support::require_unmounted(&fs_support::mounts()?, self.number, &self.path)?;
        let metadata = fs::symlink_metadata(XFS_FORMATTER)?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
        {
            return Err(io::Error::other(
                "XFS formatter must be a protected root-owned executable regular file",
            ));
        }
        let device = inherit_descriptor(&self.device)?;
        let _lock = inherit_descriptor(lock)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", device.as_raw_fd()));
        // exec preserves the alarm, pinned device and flock; there is no detached root child.
        Err(formatter_command(&path).exec())
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
        let filesystem = match self.filesystem {
            Filesystem::Ext4 => c"ext4",
            Filesystem::Xfs => c"xfs",
        };
        // SAFETY: all strings are live NUL-terminated strings. Source and target are pinned descriptors.
        // nosuid/nodev restrict the privilege grant; filesystem durability defaults are unchanged.
        let result = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                filesystem.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                std::ptr::null(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let result = self
            .mount_id()
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

    fn mount_id(&self) -> io::Result<u64> {
        let mounts = fs_support::mounts()?;
        match self.filesystem {
            Filesystem::Ext4 => fs_support::require_mounted(&mounts, self.number, &self.path),
            Filesystem::Xfs => {
                fs_support::require_mounted_for(&mounts, self.number, &self.path, Filesystem::Xfs)
            }
        }
    }

    fn rollback_mount(&self) -> io::Result<()> {
        let mounted = Self {
            filesystem: self.filesystem,
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
        let id = self.mount_id()?;
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

fn inherit_descriptor(file: &File) -> io::Result<OwnedFd> {
    // F_DUPFD retains the descriptor across exec and avoids the standard I/O slots.
    // SAFETY: file owns a live descriptor; fcntl returns a new descriptor or -1.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new, uniquely owned descriptor above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn formatter_command(device: &Path) -> Command {
    let mut command = Command::new(XFS_FORMATTER);
    command
        .arg("--")
        .arg(device)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .current_dir("/")
        .stdin(Stdio::null());
    command
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

fn verified_device(id: u32, uid: u32, filesystem: Filesystem) -> io::Result<File> {
    let class_path = PathBuf::from(format!("/sys/class/ublk-char/ublkc{id}"));
    let class = directory(&class_path.canonicalize()?)?;
    let original = class.metadata()?;
    let control = UblkCtrl::new_simple(i32::try_from(id).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    require_owner(&control, uid, filesystem)?;
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
    require_owner(&control, uid, filesystem)?;
    let current = fs::metadata(&class_path)?;
    if current.dev() != original.dev() || current.ino() != original.ino() {
        return Err(io::Error::other(
            "ublk identity changed while opening device",
        ));
    }
    Ok(device)
}

fn require_owner(control: &UblkCtrl, uid: u32, filesystem: Filesystem) -> io::Result<()> {
    let info = control.dev_info();
    let mut params = libublk::sys::ublk_params::default();
    control.get_params(&mut params).map_err(io::Error::other)?;
    validate_device_parameters(info.owner_uid, info.flags, &params, uid, filesystem)
}

fn validate_device_parameters(
    owner: u32,
    flags: u64,
    params: &libublk::sys::ublk_params,
    uid: u32,
    filesystem: Filesystem,
) -> io::Result<()> {
    if owner != uid || flags & u64::from(libublk::sys::UBLK_F_UNPRIVILEGED_DEV) == 0 {
        return Err(io::Error::other(
            "ublk device is not an unprivileged device owned by the sudo caller",
        ));
    }
    if params.basic.logical_bs_shift != 12
        || params.basic.dev_sectors != filesystem.volume_bytes() / 512
    {
        return Err(io::Error::other(
            "ublk geometry differs from the selected filesystem fixture",
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
    let lock = helper_lock()?;
    match action {
        Action::Check => println!(
            "block-storage-mount protocol=3 uid={uid} ext4={VOLUME_BYTES}/{BACKING_BYTES} xfs={}/{}",
            Filesystem::Xfs.volume_bytes(),
            Filesystem::Xfs.backing_bytes()
        ),
        Action::FormatXfs(id, name) => {
            Fixture::open(id, &name, uid, Filesystem::Xfs)?.format_xfs(&lock)?
        }
        Action::Mount(filesystem, id, name) => {
            Fixture::open(id, &name, uid, filesystem)?.mount(uid, gid)?
        }
        Action::Unmount(filesystem, id, name) => {
            Fixture::open(id, &name, uid, filesystem)?.unmount()?
        }
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
    fn formatter_uses_only_the_system_binary_and_pinned_device() {
        let path = Path::new("/proc/self/fd/17");
        let command = formatter_command(path);
        assert_eq!(command.get_program(), XFS_FORMATTER);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--", "/proc/self/fd/17"]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [
                (
                    std::ffi::OsStr::new("LC_ALL"),
                    Some(std::ffi::OsStr::new("C"))
                ),
                (
                    std::ffi::OsStr::new("PATH"),
                    Some(std::ffi::OsStr::new("/usr/sbin:/usr/bin:/sbin:/bin"))
                )
            ]
        );
    }

    #[test]
    fn inherited_descriptors_survive_exec_without_reusing_standard_io() {
        let original = File::open("/dev/null").unwrap();
        let inherited = inherit_descriptor(&original).unwrap();
        assert!(inherited.as_raw_fd() >= 3);
        let path = format!("/proc/self/fd/{}", inherited.as_raw_fd());
        assert_eq!(
            fs::metadata(&path).unwrap().rdev(),
            original.metadata().unwrap().rdev()
        );
        let status = Command::new("/bin/sh")
            .args(["-c", "test -c \"$1\"", "descriptor-test", &path])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn helper_requires_kernel_ownership_unprivileged_flag_and_exact_geometry() {
        let flags = u64::from(libublk::sys::UBLK_F_UNPRIVILEGED_DEV);
        let mut params = libublk::sys::ublk_params::default();
        params.basic.logical_bs_shift = 12;
        params.basic.dev_sectors = VOLUME_BYTES / 512;
        validate_device_parameters(1000, flags, &params, 1000, Filesystem::Ext4).unwrap();
        assert!(validate_device_parameters(1000, flags, &params, 1000, Filesystem::Xfs).is_err());
        assert!(validate_device_parameters(1001, flags, &params, 1000, Filesystem::Ext4).is_err());
        assert!(validate_device_parameters(1000, 0, &params, 1000, Filesystem::Ext4).is_err());
        params.basic.logical_bs_shift = 9;
        assert!(validate_device_parameters(1000, flags, &params, 1000, Filesystem::Ext4).is_err());
        params.basic.logical_bs_shift = 12;
        params.basic.dev_sectors += 8;
        assert!(validate_device_parameters(1000, flags, &params, 1000, Filesystem::Ext4).is_err());
        params.basic.dev_sectors = Filesystem::Xfs.volume_bytes() / 512;
        validate_device_parameters(1000, flags, &params, 1000, Filesystem::Xfs).unwrap();
        assert!(validate_device_parameters(1000, flags, &params, 1000, Filesystem::Ext4).is_err());
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
            Action::Mount(Filesystem::Ext4, 7, "my-block-storage-ublk.12.0".into())
        );
        assert_eq!(
            parse_args(&[
                "mount-xfs".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::Mount(Filesystem::Xfs, 7, "my-block-storage-ublk.12.0".into())
        );
        assert_eq!(
            parse_args(&[
                "unmount-xfs".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::Unmount(Filesystem::Xfs, 7, "my-block-storage-ublk.12.0".into())
        );
        assert_eq!(
            parse_args(&[
                "format-xfs".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::FormatXfs(7, "my-block-storage-ublk.12.0".into())
        );
        for args in [
            vec!["format-xfs", "/dev/sda", "my-block-storage-ublk.12.0"],
            vec!["format-xfs", "7", "my-block-storage-ublk.12.0", "-f"],
            vec!["format-xfs", "7", "../../etc"],
            vec!["format-ext4", "7", "my-block-storage-ublk.12.0"],
            vec!["mount-btrfs", "7", "my-block-storage-ublk.12.0"],
            vec!["mount-xfs", "7", "/tmp/arbitrary"],
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
