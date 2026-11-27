#[path = "fs_support/mod.rs"]
mod fs_support;

use fs_support::{BENCH_LOGICAL_BLOCK_BYTES, BENCH_VOLUME_BYTES, Filesystem, MOUNT_HELPER};
use libublk::ctrl::UblkCtrl;
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const XFS_FORMATTER: &str = "/usr/sbin/mkfs.xfs";

/// Which fixture rules bind a device and owned directory to the sudo caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureClass {
    /// Fixed lab geometry backed by the caller-owned `/tmp/<owned>/backing.img`.
    Lab,
    /// Allowlisted large geometry; the backing is not checked, only the kernel-recorded owner.
    Bench,
}

#[derive(Debug, PartialEq)]
enum Action {
    Check,
    FormatXfs(FixtureClass, u32, String),
    Mount(FixtureClass, Filesystem, u32, String),
    Unmount(FixtureClass, Filesystem, u32, String),
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
    let (lab, bench) = (FixtureClass::Lab, FixtureClass::Bench);
    let name = name.clone();
    match action.as_str() {
        "format-xfs" => Ok(Action::FormatXfs(lab, id, name)),
        "mount" => Ok(Action::Mount(lab, Filesystem::Ext4, id, name)),
        "unmount" => Ok(Action::Unmount(lab, Filesystem::Ext4, id, name)),
        "mount-xfs" => Ok(Action::Mount(lab, Filesystem::Xfs, id, name)),
        "unmount-xfs" => Ok(Action::Unmount(lab, Filesystem::Xfs, id, name)),
        "bench-format-xfs" => Ok(Action::FormatXfs(bench, id, name)),
        "bench-mount" => Ok(Action::Mount(bench, Filesystem::Ext4, id, name)),
        "bench-unmount" => Ok(Action::Unmount(bench, Filesystem::Ext4, id, name)),
        "bench-mount-xfs" => Ok(Action::Mount(bench, Filesystem::Xfs, id, name)),
        "bench-unmount-xfs" => Ok(Action::Unmount(bench, Filesystem::Xfs, id, name)),
        _ => Err(usage()),
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "usage: {MOUNT_HELPER} check | [bench-]format-xfs|[bench-]mount|[bench-]unmount|[bench-]mount-xfs|[bench-]unmount-xfs <ublk-id> <owned-directory-name>"
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
    class: FixtureClass,
    filesystem: Filesystem,
    parent: File,
    target: File,
    device: File,
    path: PathBuf,
    number: (u32, u32),
}

impl Fixture {
    fn open(
        class: FixtureClass,
        id: u32,
        name: &str,
        uid: u32,
        filesystem: Filesystem,
    ) -> io::Result<Self> {
        let path = Path::new("/tmp").join(name).join("mount");
        let parent = directory(path.parent().ok_or_else(usage)?)?;
        let metadata = parent.metadata()?;
        if metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
            return Err(io::Error::other(
                "fixture parent must be caller-owned mode 0700",
            ));
        }
        if class == FixtureClass::Lab {
            require_lab_backing(&parent, uid, filesystem)?;
        }
        let device = verified_device(id, uid, filesystem, class)?;
        let metadata = device.metadata()?;
        let number = (libc::major(metadata.rdev()), libc::minor(metadata.rdev()));
        let target = directory(&fd_path(&parent).join("mount"))?;
        Ok(Self {
            class,
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
        let data = mount_data(self.class).map_or(std::ptr::null(), |data| data.as_ptr().cast());
        // SAFETY: all strings are live NUL-terminated strings; data is null or a static string.
        // Source and target are pinned descriptors. nosuid/nodev restrict the privilege grant;
        // filesystem durability defaults are unchanged.
        let result = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                filesystem.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                data,
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
            class: self.class,
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

fn require_lab_backing(parent: &File, uid: u32, filesystem: Filesystem) -> io::Result<()> {
    let backing = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(fd_path(parent).join("backing.img"))?;
    let metadata = backing.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.len() != filesystem.backing_bytes()
    {
        return Err(io::Error::other(
            "fixture backing is not the caller-owned bounded regular file",
        ));
    }
    Ok(())
}

/// Mount data string for the fixture class; the mount flags are the same for both classes.
fn mount_data(class: FixtureClass) -> Option<&'static CStr> {
    match class {
        FixtureClass::Lab => None,
        // Both ext4 and XFS then forward freed extents to the device as discards.
        FixtureClass::Bench => Some(c"discard"),
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

fn verified_device(
    id: u32,
    uid: u32,
    filesystem: Filesystem,
    fixture: FixtureClass,
) -> io::Result<File> {
    let class_path = PathBuf::from(format!("/sys/class/ublk-char/ublkc{id}"));
    let class = directory(&class_path.canonicalize()?)?;
    let original = class.metadata()?;
    let control = UblkCtrl::new_simple(i32::try_from(id).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    require_owner(&control, uid, filesystem, fixture)?;
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
    require_owner(&control, uid, filesystem, fixture)?;
    let current = fs::metadata(&class_path)?;
    if current.dev() != original.dev() || current.ino() != original.ino() {
        return Err(io::Error::other(
            "ublk identity changed while opening device",
        ));
    }
    Ok(device)
}

fn require_owner(
    control: &UblkCtrl,
    uid: u32,
    filesystem: Filesystem,
    class: FixtureClass,
) -> io::Result<()> {
    let info = control.dev_info();
    let mut params = libublk::sys::ublk_params::default();
    control.get_params(&mut params).map_err(io::Error::other)?;
    let owner = DeviceOwner {
        uid: info.owner_uid,
        flags: info.flags,
    };
    validate_device_parameters(owner, &params, uid, filesystem, class)
}

/// Kernel-recorded ublk owner and feature flags.
#[derive(Clone, Copy)]
struct DeviceOwner {
    uid: u32,
    flags: u64,
}

fn validate_device_parameters(
    owner: DeviceOwner,
    params: &libublk::sys::ublk_params,
    uid: u32,
    filesystem: Filesystem,
    class: FixtureClass,
) -> io::Result<()> {
    if owner.uid != uid || owner.flags & u64::from(libublk::sys::UBLK_F_UNPRIVILEGED_DEV) == 0 {
        return Err(io::Error::other(
            "ublk device is not an unprivileged device owned by the sudo caller",
        ));
    }
    let basic = &params.basic;
    let geometry_matches = match class {
        FixtureClass::Lab => {
            basic.logical_bs_shift == 12 && basic.dev_sectors == filesystem.volume_bytes() / 512
        }
        FixtureClass::Bench => {
            1_u64.checked_shl(u32::from(basic.logical_bs_shift)) == Some(BENCH_LOGICAL_BLOCK_BYTES)
                && BENCH_VOLUME_BYTES.contains(&basic.dev_sectors.saturating_mul(512))
        }
    };
    if !geometry_matches {
        return Err(io::Error::other(format!(
            "ublk geometry differs from the selected {class:?} filesystem fixture"
        )));
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
            "block-storage-mount {}",
            fs_support::helper_check_fields(uid)
        ),
        Action::FormatXfs(class, id, name) => {
            Fixture::open(class, id, &name, uid, Filesystem::Xfs)?.format_xfs(&lock)?
        }
        Action::Mount(class, filesystem, id, name) => {
            Fixture::open(class, id, &name, uid, filesystem)?.mount(uid, gid)?
        }
        Action::Unmount(class, filesystem, id, name) => {
            Fixture::open(class, id, &name, uid, filesystem)?.unmount()?
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

    const OWNED: &str = "my-block-storage-ublk.12.0";

    fn unprivileged(uid: u32) -> DeviceOwner {
        DeviceOwner {
            uid,
            flags: u64::from(libublk::sys::UBLK_F_UNPRIVILEGED_DEV),
        }
    }

    fn geometry(logical_bs_shift: u8, bytes: u64) -> libublk::sys::ublk_params {
        let mut params = libublk::sys::ublk_params::default();
        params.basic.logical_bs_shift = logical_bs_shift;
        params.basic.dev_sectors = bytes / 512;
        params
    }

    fn parse(args: &[&str]) -> io::Result<Action> {
        parse_args(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn helper_requires_kernel_ownership_unprivileged_flag_and_exact_geometry() {
        let lab = FixtureClass::Lab;
        let owner = unprivileged(1000);
        let mut params = geometry(12, fs_support::VOLUME_BYTES);
        validate_device_parameters(owner, &params, 1000, Filesystem::Ext4, lab).unwrap();
        assert!(validate_device_parameters(owner, &params, 1000, Filesystem::Xfs, lab).is_err());
        assert!(
            validate_device_parameters(unprivileged(1001), &params, 1000, Filesystem::Ext4, lab)
                .is_err()
        );
        let privileged = DeviceOwner {
            uid: 1000,
            flags: 0,
        };
        assert!(
            validate_device_parameters(privileged, &params, 1000, Filesystem::Ext4, lab).is_err()
        );
        params.basic.logical_bs_shift = 9;
        assert!(validate_device_parameters(owner, &params, 1000, Filesystem::Ext4, lab).is_err());
        params.basic.logical_bs_shift = 12;
        params.basic.dev_sectors += 8;
        assert!(validate_device_parameters(owner, &params, 1000, Filesystem::Ext4, lab).is_err());
        params.basic.dev_sectors = Filesystem::Xfs.volume_bytes() / 512;
        validate_device_parameters(owner, &params, 1000, Filesystem::Xfs, lab).unwrap();
        assert!(validate_device_parameters(owner, &params, 1000, Filesystem::Ext4, lab).is_err());
    }

    #[test]
    fn lab_fixture_rejects_bench_geometry() {
        for bytes in BENCH_VOLUME_BYTES {
            for filesystem in [Filesystem::Ext4, Filesystem::Xfs] {
                let params = geometry(12, bytes);
                assert!(
                    validate_device_parameters(
                        unprivileged(1000),
                        &params,
                        1000,
                        filesystem,
                        FixtureClass::Lab
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn bench_fixture_accepts_each_allowlisted_size_for_both_filesystems() {
        assert_eq!(
            BENCH_VOLUME_BYTES,
            [4, 8, 16, 32, 64].map(|gib: u64| gib << 30)
        );
        for bytes in BENCH_VOLUME_BYTES {
            for filesystem in [Filesystem::Ext4, Filesystem::Xfs] {
                let params = geometry(12, bytes);
                validate_device_parameters(
                    unprivileged(1000),
                    &params,
                    1000,
                    filesystem,
                    FixtureClass::Bench,
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn bench_fixture_rejects_unlisted_size_block_size_owner_and_privileged_device() {
        let bench = FixtureClass::Bench;
        let owner = unprivileged(1000);
        for bytes in [
            fs_support::VOLUME_BYTES,
            Filesystem::Xfs.volume_bytes(),
            2 << 30,
            (4 << 30) + 4096,
            (64 << 30) - 4096,
            128 << 30,
            0,
        ] {
            let params = geometry(12, bytes);
            assert!(
                validate_device_parameters(owner, &params, 1000, Filesystem::Ext4, bench).is_err()
            );
        }
        let mut params = geometry(12, 8 << 30);
        params.basic.dev_sectors = u64::MAX;
        assert!(validate_device_parameters(owner, &params, 1000, Filesystem::Xfs, bench).is_err());
        for shift in [9, 11, 13, 64, 255] {
            let params = geometry(shift, 8 << 30);
            assert!(
                validate_device_parameters(owner, &params, 1000, Filesystem::Xfs, bench).is_err()
            );
        }
        let params = geometry(12, 8 << 30);
        assert!(
            validate_device_parameters(unprivileged(1001), &params, 1000, Filesystem::Xfs, bench)
                .is_err()
        );
        let privileged = DeviceOwner {
            uid: 1000,
            flags: 0,
        };
        assert!(
            validate_device_parameters(privileged, &params, 1000, Filesystem::Xfs, bench).is_err()
        );
    }

    #[test]
    fn bench_mounts_request_discard_and_lab_mounts_pass_no_data() {
        assert_eq!(mount_data(FixtureClass::Lab), None);
        assert_eq!(mount_data(FixtureClass::Bench), Some(c"discard"));
    }

    #[test]
    fn helper_accepts_bench_actions_with_the_lab_argument_shape() {
        let (bench, ext4, xfs) = (FixtureClass::Bench, Filesystem::Ext4, Filesystem::Xfs);
        for (action, expected) in [
            ("bench-mount", Action::Mount(bench, ext4, 7, OWNED.into())),
            (
                "bench-unmount",
                Action::Unmount(bench, ext4, 7, OWNED.into()),
            ),
            (
                "bench-mount-xfs",
                Action::Mount(bench, xfs, 7, OWNED.into()),
            ),
            (
                "bench-unmount-xfs",
                Action::Unmount(bench, xfs, 7, OWNED.into()),
            ),
            (
                "bench-format-xfs",
                Action::FormatXfs(bench, 7, OWNED.into()),
            ),
        ] {
            assert_eq!(parse(&[action, "7", OWNED]).unwrap(), expected);
        }
    }

    #[test]
    fn helper_rejects_malformed_bench_actions() {
        for args in [
            vec!["bench-mount", "/dev/sda", OWNED],
            vec!["bench-mount", "7", "/tmp/arbitrary"],
            vec!["bench-mount-xfs", "7", "../../etc"],
            vec!["bench-unmount", "7", OWNED, "-f"],
            vec!["bench-unmount-xfs", "7", OWNED, "-l"],
            vec!["bench-format-xfs", "7", OWNED, "-f"],
            vec!["bench-format-xfs", "4294967295", OWNED],
            vec!["bench-format-ext4", "7", OWNED],
            vec!["bench-mount-btrfs", "7", OWNED],
            vec!["bench-mount", "7", OWNED, "-o", "suid"],
            vec!["bench-mount", "7", OWNED, "68719476736"],
            vec!["bench", "7", OWNED],
            vec!["bench-check"],
            vec!["bench-mount"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn helper_rejects_paths_options_and_arbitrary_device_names() {
        let lab = FixtureClass::Lab;
        assert_eq!(parse_args(&["check".into()]).unwrap(), Action::Check);
        assert_eq!(
            parse_args(&[
                "mount".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::Mount(
                lab,
                Filesystem::Ext4,
                7,
                "my-block-storage-ublk.12.0".into()
            )
        );
        assert_eq!(
            parse(&["unmount", "7", OWNED]).unwrap(),
            Action::Unmount(lab, Filesystem::Ext4, 7, OWNED.into())
        );
        assert_eq!(
            parse_args(&[
                "mount-xfs".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::Mount(lab, Filesystem::Xfs, 7, "my-block-storage-ublk.12.0".into())
        );
        assert_eq!(
            parse_args(&[
                "unmount-xfs".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::Unmount(lab, Filesystem::Xfs, 7, "my-block-storage-ublk.12.0".into())
        );
        assert_eq!(
            parse_args(&[
                "format-xfs".into(),
                "7".into(),
                "my-block-storage-ublk.12.0".into()
            ])
            .unwrap(),
            Action::FormatXfs(lab, 7, "my-block-storage-ublk.12.0".into())
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
