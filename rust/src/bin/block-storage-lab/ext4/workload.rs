use crate::fs_support;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

// Small enough for the finite log, but includes partial filesystem-block writes and truncation.
const INITIAL_BYTES: usize = 1024 * 1024;
const RETAINED_BYTES: usize = 60_013;
const FINAL_BYTES: usize = 70_001;
const OVERWRITE_OFFSET: usize = 32 * 1024;
const OVERWRITE_BYTES: usize = 4096;
const STABLE_BYTES: usize = 256 * 1024;

fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 31 + index / 4096) % 251) as u8)
        .collect()
}

fn expected_renamed() -> Vec<u8> {
    let mut bytes = pattern(RETAINED_BYTES);
    bytes[OVERWRITE_OFFSET..OVERWRITE_OFFSET + OVERWRITE_BYTES].fill(0xa7);
    bytes.resize(FINAL_BYTES, 0);
    bytes
}

pub(super) fn manifest() -> String {
    format!(
        "a/stable bytes={STABLE_BYTES} xxh3={:016x}\nb/renamed bytes={FINAL_BYTES} xxh3={:016x}\nabsent: a/payload a/deleted\n",
        xxh3_64(&pattern(STABLE_BYTES)),
        xxh3_64(&expected_renamed())
    )
}

fn create_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(super) fn populate(root: &Path) -> io::Result<()> {
    if fs::read_dir(root)?.next().is_some() {
        return Err(io::Error::other("workload directory is not empty"));
    }
    let a = root.join("a");
    let b = root.join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    File::open(root)?.sync_all()?;
    create_synced(&a.join("stable"), &pattern(STABLE_BYTES))?;
    create_synced(&a.join("payload"), &pattern(INITIAL_BYTES))?;
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(a.join("payload"))?;
    file.seek(SeekFrom::Start(OVERWRITE_OFFSET as u64))?;
    file.write_all(&[0xa7; OVERWRITE_BYTES])?;
    file.sync_all()?;
    fs::rename(a.join("payload"), b.join("renamed"))?;
    File::open(&a)?.sync_all()?;
    File::open(&b)?.sync_all()?;
    file.set_len(RETAINED_BYTES as u64)?;
    file.sync_all()?;
    file.set_len(FINAL_BYTES as u64)?;
    file.sync_all()?;
    create_synced(&a.join("deleted"), b"this file must disappear")?;
    File::open(&a)?.sync_all()?;
    fs::remove_file(a.join("deleted"))?;
    File::open(&a)?.sync_all()?;
    File::open(&b)?.sync_all()?;
    File::open(root)?.sync_all()?;
    verify(root)
}

fn require_entries(root: &Path, expected: &[&str]) -> io::Result<()> {
    let mut entries = fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    let mut expected: Vec<_> = expected.iter().map(std::ffi::OsString::from).collect();
    expected.sort();
    if entries != expected {
        return Err(io::Error::other(format!(
            "unexpected directory entries in {}",
            root.display()
        )));
    }
    Ok(())
}

fn verify_file(path: &Path, expected: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected.len() as u64 {
        return Err(io::Error::other(format!(
            "wrong type or size: {}",
            path.display()
        )));
    }
    let mut actual = vec![0; expected.len()];
    file.read_exact(&mut actual)?;
    if actual != expected || xxh3_64(&actual) != xxh3_64(expected) {
        return Err(io::Error::other(format!(
            "content mismatch: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn verify(root: &Path) -> io::Result<()> {
    require_entries(root, &["a", "b"])?;
    for name in ["a", "b"] {
        if !fs::symlink_metadata(root.join(name))?.is_dir() {
            return Err(io::Error::other("workload directory was replaced"));
        }
    }
    require_entries(&root.join("a"), &["stable"])?;
    require_entries(&root.join("b"), &["renamed"])?;
    verify_file(&root.join("a/stable"), &pattern(STABLE_BYTES))?;
    verify_file(&root.join("b/renamed"), &expected_renamed())
}

pub fn worker(populate_files: bool) -> io::Result<()> {
    let root = std::env::current_dir()?;
    let mount = root
        .parent()
        .ok_or_else(|| io::Error::other("missing mount parent"))?;
    let owned = mount
        .parent()
        .ok_or_else(|| io::Error::other("missing fixture parent"))?;
    if root.file_name() != Some(std::ffi::OsStr::new("work"))
        || mount.file_name() != Some(std::ffi::OsStr::new("mount"))
        || owned.parent() != Some(Path::new("/tmp"))
        || !owned
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(fs_support::owned_name)
    {
        return Err(io::Error::other(
            "internal worker requires the lab-owned ext4 work directory",
        ));
    }
    let metadata = fs::metadata(&root)?;
    let device = (libc::major(metadata.dev()), libc::minor(metadata.dev()));
    fs_support::require_mounted(&fs_support::mounts()?, device, mount)?;
    let sys_device = fs::canonicalize(format!("/sys/dev/block/{}:{}", device.0, device.1))?;
    if !sys_device
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.strip_prefix("ublkb")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        })
    {
        return Err(io::Error::other("worker filesystem is not backed by ublk"));
    }
    if populate_files {
        populate(&root)?;
    } else {
        verify(&root)?;
    }
    print!("{}", manifest());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ublk::OwnedTempDir;

    #[test]
    fn workload_verifies_exact_contents_and_rejects_corruption() {
        let mut owned = OwnedTempDir::create(&std::env::temp_dir()).unwrap();
        let root = owned.path().join("work");
        fs::create_dir(&root).unwrap();
        populate(&root).unwrap();
        verify(&root).unwrap();
        assert!(populate(&root).is_err());
        let file = root.join("b/renamed");
        fs::write(&file, vec![0; FINAL_BYTES]).unwrap();
        assert!(verify(&root).is_err());
        fs::write(&file, expected_renamed()).unwrap();
        fs::write(root.join("a/deleted"), b"unexpected").unwrap();
        assert!(verify(&root).is_err());
        fs::remove_file(root.join("a/deleted")).unwrap();
        fs::remove_file(root.join("a/stable")).unwrap();
        std::os::unix::fs::symlink(&file, root.join("a/stable")).unwrap();
        assert!(verify(&root).is_err());
        fs::remove_dir_all(&root).unwrap();
        owned.cleanup().unwrap();
    }
}
