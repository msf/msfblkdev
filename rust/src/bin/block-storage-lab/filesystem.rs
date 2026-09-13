use crate::fs_support::{Filesystem, MOUNT_HELPER};
use crate::ublk::DevicePath;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::Command;

const MAX_SYSTEM_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn mkfs_command(
    filesystem: Filesystem,
    device: &DevicePath,
    owned_name: &OsStr,
) -> Command {
    match filesystem {
        Filesystem::Ext4 => {
            let mut command = Command::new("mkfs.ext4");
            command.arg(device.as_path());
            command
        }
        Filesystem::Xfs => helper_command("format-xfs", device, owned_name),
    }
}

pub(crate) fn helper_command(action: &str, device: &DevicePath, owned_name: &OsStr) -> Command {
    let mut command = Command::new("sudo");
    command
        .args(["-n", "--", MOUNT_HELPER, action])
        .arg(device.id().to_string())
        .arg(owned_name);
    command
}

pub(crate) fn check_command(filesystem: Filesystem, device: &Path) -> Command {
    let mut command = Command::new(match filesystem {
        Filesystem::Ext4 => "e2fsck",
        Filesystem::Xfs => "/usr/sbin/xfs_repair",
    });
    match filesystem {
        Filesystem::Ext4 => {
            command.args(["-f", "-n"]);
        }
        Filesystem::Xfs => {
            command.arg("-n");
        }
    }
    command.arg(device);
    command
}

pub(crate) fn version_commands(filesystem: Filesystem) -> [(&'static str, Command); 2] {
    match filesystem {
        Filesystem::Ext4 => [
            ("mkfs-version", command_with_arg("mkfs.ext4", "-V")),
            ("e2fsck-version", command_with_arg("e2fsck", "-V")),
        ],
        Filesystem::Xfs => [
            ("mkfs-version", command_with_arg("/usr/sbin/mkfs.xfs", "-V")),
            (
                "xfs-repair-version",
                command_with_arg("/usr/sbin/xfs_repair", "-V"),
            ),
        ],
    }
}

fn command_with_arg(program: &str, argument: &str) -> Command {
    let mut command = Command::new(program);
    command.arg(argument);
    command
}

pub(crate) const fn helper_action(filesystem: Filesystem, mount: bool) -> &'static str {
    match (filesystem, mount) {
        (Filesystem::Ext4, true) => "mount",
        (Filesystem::Ext4, false) => "unmount",
        (Filesystem::Xfs, true) => "mount-xfs",
        (Filesystem::Xfs, false) => "unmount-xfs",
    }
}

pub(crate) const fn worker_command(filesystem: Filesystem, populate: bool) -> &'static str {
    match (filesystem, populate) {
        (Filesystem::Ext4, true) => "ext4-populate",
        (Filesystem::Ext4, false) => "ext4-verify",
        (Filesystem::Xfs, true) => "xfs-populate",
        (Filesystem::Xfs, false) => "xfs-verify",
    }
}

pub(crate) fn check_xfs_resources(path: &Path) -> io::Result<(u64, u64)> {
    let available_memory = available_memory_bytes()?;
    let available_space = available_space_bytes(path)?;
    if available_memory < Filesystem::Xfs.volume_bytes() {
        return Err(io::Error::other(format!(
            "insufficient available memory for bounded XFS fixture: {available_memory} bytes"
        )));
    }
    if available_space < Filesystem::Xfs.backing_bytes() {
        return Err(io::Error::other(format!(
            "insufficient temporary space for XFS backing ceiling: {available_space} bytes"
        )));
    }
    Ok((available_memory, available_space))
}

fn available_memory_bytes() -> io::Result<u64> {
    let text = read_bounded(Path::new("/proc/meminfo"))?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .ok_or_else(|| io::Error::other("MemAvailable is absent from /proc/meminfo"))?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("MemAvailable:") {
        return Err(io::Error::other("invalid MemAvailable record"));
    }
    let kib = fields
        .next()
        .ok_or_else(|| io::Error::other("missing MemAvailable value"))?
        .parse::<u64>()
        .map_err(io::Error::other)?;
    if fields.next() != Some("kB") || fields.next().is_some() {
        return Err(io::Error::other("invalid MemAvailable unit"));
    }
    kib.checked_mul(1024)
        .ok_or_else(|| io::Error::other("MemAvailable overflows bytes"))
}

fn available_space_bytes(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: path is a live NUL-terminated string and stats points to writable storage.
    let mut stats = unsafe { std::mem::zeroed::<libc::statvfs>() };
    // SAFETY: both pointers remain valid for the duration of statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return Err(io::Error::last_os_error());
    }
    stats
        .f_bavail
        .checked_mul(stats.f_frsize)
        .ok_or_else(|| io::Error::other("available temporary space overflows bytes"))
}

pub(crate) fn xfs_kernel_supported() -> io::Result<bool> {
    Ok(read_bounded(Path::new("/proc/filesystems"))?
        .lines()
        .any(|line| line.split_whitespace().last() == Some("xfs")))
}

pub(crate) fn effective_mount_options(mount_id: u64) -> io::Result<String> {
    parse_effective_mount_options(&read_bounded(Path::new("/proc/self/mountinfo"))?, mount_id)
}

fn parse_effective_mount_options(text: &str, mount_id: u64) -> io::Result<String> {
    let id = mount_id.to_string();
    let mut matches = text
        .lines()
        .filter(|line| line.split_whitespace().next() == Some(id.as_str()));
    let line = matches
        .next()
        .ok_or_else(|| io::Error::other("recorded mount is absent from mountinfo"))?;
    if matches.next().is_some() {
        return Err(io::Error::other("duplicate mount ID in mountinfo"));
    }
    let (left, right) = line
        .split_once(" - ")
        .ok_or_else(|| io::Error::other("invalid mountinfo record"))?;
    let mount_options = left
        .split_whitespace()
        .nth(5)
        .ok_or_else(|| io::Error::other("mount options absent from mountinfo"))?;
    let mut right = right.split_whitespace();
    let filesystem = right
        .next()
        .ok_or_else(|| io::Error::other("filesystem absent from mountinfo"))?;
    let _source = right
        .next()
        .ok_or_else(|| io::Error::other("source absent from mountinfo"))?;
    let super_options = right
        .next()
        .ok_or_else(|| io::Error::other("super options absent from mountinfo"))?;
    Ok(format!(
        "filesystem={filesystem} mount_options={mount_options} super_options={super_options}"
    ))
}

fn read_bounded(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(MAX_SYSTEM_FILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SYSTEM_FILE_BYTES {
        return Err(io::Error::other(format!(
            "{} exceeds the read limit",
            path.display()
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_commands_keep_defaults_and_checks_read_only() {
        let device: DevicePath = "/dev/ublkb17".parse().unwrap();
        let path = device.as_path();
        let name = OsStr::new("my-block-storage-ublk.12.0");
        let ext4 = mkfs_command(Filesystem::Ext4, &device, name);
        assert_eq!(ext4.get_program(), "mkfs.ext4");
        assert_eq!(ext4.get_args().collect::<Vec<_>>(), [path.as_os_str()]);
        let ext4_check = check_command(Filesystem::Ext4, path);
        assert_eq!(ext4_check.get_program(), "e2fsck");
        assert_eq!(
            ext4_check.get_args().collect::<Vec<_>>(),
            ["-f", "-n", "/dev/ublkb17"]
        );
        let xfs = mkfs_command(Filesystem::Xfs, &device, name);
        assert_eq!(xfs.get_program(), "sudo");
        assert_eq!(
            xfs.get_args().collect::<Vec<_>>(),
            [
                "-n",
                "--",
                MOUNT_HELPER,
                "format-xfs",
                "17",
                "my-block-storage-ublk.12.0"
            ]
        );
        let xfs_check = check_command(Filesystem::Xfs, path);
        assert_eq!(xfs_check.get_program(), "/usr/sbin/xfs_repair");
        assert_eq!(
            xfs_check.get_args().collect::<Vec<_>>(),
            ["-n", "/dev/ublkb17"]
        );
    }

    #[test]
    fn filesystem_selects_helper_and_worker_commands() {
        assert_eq!(helper_action(Filesystem::Ext4, true), "mount");
        assert_eq!(helper_action(Filesystem::Ext4, false), "unmount");
        assert_eq!(helper_action(Filesystem::Xfs, true), "mount-xfs");
        assert_eq!(helper_action(Filesystem::Xfs, false), "unmount-xfs");
        assert_eq!(worker_command(Filesystem::Xfs, true), "xfs-populate");
        assert_eq!(worker_command(Filesystem::Xfs, false), "xfs-verify");
    }

    #[test]
    fn effective_options_come_from_the_recorded_mountinfo_entry() {
        let text = "41 1 0:1 / /elsewhere rw - tmpfs tmpfs rw\n\
                    42 1 259:7 / /tmp/owned/mount rw,nosuid,nodev - xfs /dev/ublkb3 rw,attr2,inode64\n";
        assert_eq!(
            parse_effective_mount_options(text, 42).unwrap(),
            "filesystem=xfs mount_options=rw,nosuid,nodev super_options=rw,attr2,inode64"
        );
        assert!(parse_effective_mount_options(text, 43).is_err());
    }
}
