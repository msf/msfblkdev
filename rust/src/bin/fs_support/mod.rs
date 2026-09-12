use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub const MOUNT_HELPER: &str = "/usr/local/libexec/block-storage-mount";
pub const VOLUME_BYTES: u64 = 128 * 1024 * 1024;
pub const BACKING_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct Mount {
    pub id: u64,
    pub device: (u32, u32),
    pub root: PathBuf,
    pub target: PathBuf,
    pub filesystem: String,
}

pub fn mounts() -> io::Result<Vec<Mount>> {
    // Reject an unexpectedly large mount table before allocating an unbounded privileged snapshot.
    const MAX_MOUNTINFO_BYTES: u64 = 16 * 1024 * 1024;
    let mut text = String::new();
    fs::File::open("/proc/self/mountinfo")?
        .take(MAX_MOUNTINFO_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_MOUNTINFO_BYTES {
        return Err(io::Error::other("mount table exceeds the lab limit"));
    }
    parse_mounts(&text)
}

pub fn parse_mounts(text: &str) -> io::Result<Vec<Mount>> {
    text.lines().map(parse_mount).collect()
}

fn parse_mount(line: &str) -> io::Result<Mount> {
    let (left, right) = line.split_once(" - ").ok_or_else(invalid_mount)?;
    let fields: Vec<_> = left.split_whitespace().collect();
    if fields.len() < 6 {
        return Err(invalid_mount());
    }
    let (major, minor) = fields[2].split_once(':').ok_or_else(invalid_mount)?;
    Ok(Mount {
        id: fields[0].parse().map_err(|_| invalid_mount())?,
        device: (
            major.parse().map_err(|_| invalid_mount())?,
            minor.parse().map_err(|_| invalid_mount())?,
        ),
        root: decode_path(fields[3])?,
        target: decode_path(fields[4])?,
        filesystem: right
            .split_whitespace()
            .next()
            .ok_or_else(invalid_mount)?
            .to_owned(),
    })
}

fn decode_path(value: &str) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let digits: Vec<_> = input.by_ref().take(3).collect();
        let escaped = match digits.as_slice() {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => return Err(invalid_mount()),
        };
        bytes.push(escaped);
    }
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

fn invalid_mount() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid mountinfo record")
}

pub fn require_unmounted(table: &[Mount], device: (u32, u32), target: &Path) -> io::Result<()> {
    if table
        .iter()
        .any(|mount| mount.device == device || mount.target.starts_with(target))
    {
        return Err(io::Error::other(
            "device or owned mount directory is already mounted",
        ));
    }
    Ok(())
}

pub fn require_mounted(table: &[Mount], device: (u32, u32), target: &Path) -> io::Result<u64> {
    let related: Vec<_> = table
        .iter()
        .filter(|mount| mount.device == device || mount.target.starts_with(target))
        .collect();
    let [mount] = related.as_slice() else {
        return Err(io::Error::other(
            "missing, stacked, nested, or aliased mount; refusing operation",
        ));
    };
    if mount.device != device
        || mount.target != target
        || mount.root != Path::new("/")
        || mount.filesystem != "ext4"
    {
        return Err(io::Error::other(
            "mount source, root, target, or filesystem differs from recorded ext4 device",
        ));
    }
    Ok(mount.id)
}

pub fn owned_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("my-block-storage-ublk.") else {
        return false;
    };
    let Some((pid, nonce)) = suffix.split_once('.') else {
        return false;
    };
    [pid, nonce]
        .iter()
        .all(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_checks_reject_wrong_source_aliases_and_nested_mounts() {
        let line = "42 1 259:7 / /tmp/owned/mount rw - ext4 /dev/ublkb3 rw\n";
        let table = parse_mounts(line).unwrap();
        let target = Path::new("/tmp/owned/mount");
        assert_eq!(require_mounted(&table, (259, 7), target).unwrap(), 42);
        assert!(require_unmounted(&table, (259, 7), target).is_err());
        assert!(require_mounted(&table, (259, 8), target).is_err());
        for other in [
            "43 1 259:7 / /elsewhere rw - ext4 /dev/ublkb3 rw\n",
            "43 42 0:8 / /tmp/owned/mount/nested rw - tmpfs tmpfs rw\n",
            line,
        ] {
            let table = parse_mounts(&format!("{line}{other}")).unwrap();
            assert!(require_mounted(&table, (259, 7), target).is_err());
        }
        assert!(require_unmounted(&[], (259, 7), target).is_ok());
        assert!(require_mounted(&[], (259, 7), target).is_err());
        for changed in [
            line.replace(" / /tmp", " /subdir /tmp"),
            line.replace("ext4", "xfs"),
        ] {
            assert!(require_mounted(&parse_mounts(&changed).unwrap(), (259, 7), target).is_err());
        }
    }

    #[test]
    fn parses_mountinfo_escapes_and_rejects_malformed_records() {
        let table = parse_mounts("1 0 8:1 / /with\\040space rw shared:1 - ext4 /dev/a rw").unwrap();
        assert_eq!(table[0].target, Path::new("/with space"));
        for line in [
            "",
            "bad",
            "1 0 8:1 / /bad\\ rw - ext4 a rw",
            "x 0 8:1 / / rw - ext4 a rw",
        ] {
            assert!(parse_mount(line).is_err());
        }
    }

    #[test]
    fn only_generated_directory_names_are_accepted() {
        assert!(owned_name("my-block-storage-ublk.12.0"));
        for name in [
            "/tmp/my-block-storage-ublk.12.0",
            "../mount",
            "my-block-storage-ublk.12.0/mount",
            "my-block-storage-ublk..0",
            "my-block-storage-ublk.12.0.1",
        ] {
            assert!(!owned_name(name));
        }
    }
}
