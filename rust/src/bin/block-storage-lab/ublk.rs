use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

pub const BLOCK_BYTES: u64 = 4096;
pub const VOLUME_BLOCKS: u64 = 64;
pub const RECORD_CAPACITY: u64 = 32;
const OWNED_PREFIX: &str = "my-block-storage-ublk.";
const OWNED_MARKER: &str = ".owned-by-block-storage-lab";
static DIRECTORY_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceId(u32);

impl DeviceId {
    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(output)
    }
}

impl FromStr for DeviceId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("device ID must be canonical unsigned decimal");
        }
        value
            .parse()
            .map(Self)
            .map_err(|_| "device ID is out of range")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevicePath {
    id: DeviceId,
    path: PathBuf,
}

impl DevicePath {
    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl FromStr for DevicePath {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let suffix = value
            .strip_prefix("/dev/ublkb")
            .ok_or("device path must be exactly /dev/ublkbN")?;
        let id = suffix.parse()?;
        Ok(Self {
            id,
            path: PathBuf::from(value),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    pub block_bytes: u64,
    pub volume_blocks: u64,
    pub record_capacity: u64,
    pub log_start_blocks: u64,
    pub backing_blocks: u64,
}

impl Geometry {
    pub fn expected() -> Self {
        let physical_blocks = VOLUME_BLOCKS.div_ceil(1024);
        let checksum_blocks = VOLUME_BLOCKS.div_ceil(512);
        let log_start_blocks = 2 + 2 * physical_blocks + 2 * checksum_blocks;
        Self {
            block_bytes: BLOCK_BYTES,
            volume_blocks: VOLUME_BLOCKS,
            record_capacity: RECORD_CAPACITY,
            log_start_blocks,
            backing_blocks: log_start_blocks + 2 * RECORD_CAPACITY,
        }
    }

    pub fn volume_bytes(self) -> u64 {
        self.block_bytes * self.volume_blocks
    }

    pub fn backing_bytes(self) -> u64 {
        self.block_bytes * self.backing_blocks
    }

    pub fn sectors(self) -> u64 {
        self.volume_bytes() / 512
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

impl fmt::Display for FileIdentity {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}:{}", self.device, self.inode)
    }
}

pub struct OwnedTempDir {
    path: PathBuf,
    identity: FileIdentity,
    cleaned: bool,
}

impl OwnedTempDir {
    pub fn create(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        for _ in 0..100 {
            let nonce = DIRECTORY_NONCE.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("{OWNED_PREFIX}{}.{}", std::process::id(), nonce));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => match initialize_owned_dir(path.clone()) {
                    Ok(owned) => return Ok(owned),
                    Err(error) => {
                        let _ = fs::remove_file(path.join(OWNED_MARKER));
                        if let Err(remove_error) = fs::remove_dir(&path) {
                            return Err(io::Error::other(format!(
                                "initializing temporary directory failed: {error}; rollback failed: {remove_error}"
                            )));
                        }
                        return Err(error);
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary directory",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub fn validate(&self) -> io::Result<()> {
        validate_owned_dir(&self.path, self.identity)
    }

    pub fn preserve(&mut self) {
        self.cleaned = true;
    }

    pub fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.validate()?;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_name() == OWNED_MARKER {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            // Flat regular files are the only resources this harness owns.
            if !metadata.is_file()
                || metadata.uid() != effective_uid()
                || metadata.dev() != self.identity.device
            {
                return Err(io::Error::other(format!(
                    "refusing to remove unexpected temporary resource {}",
                    entry.path().display()
                )));
            }
            fs::remove_file(entry.path())?;
        }
        fs::remove_file(self.path.join(OWNED_MARKER))?;
        fs::remove_dir(&self.path)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for OwnedTempDir {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = self.cleanup()
        {
            eprintln!(
                "refusing to remove unvalidated temporary directory {}: {error}",
                self.path.display()
            );
        }
    }
}

fn initialize_owned_dir(path: PathBuf) -> io::Result<OwnedTempDir> {
    let metadata = fs::symlink_metadata(&path)?;
    let identity = FileIdentity::from_metadata(&metadata);
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.join(OWNED_MARKER))?;
    writeln!(marker, "{identity}")?;
    Ok(OwnedTempDir {
        path,
        identity,
        cleaned: false,
    })
}

fn validate_owned_dir(path: &Path, expected: FileIdentity) -> io::Result<()> {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    if !name.starts_with(OWNED_PREFIX) || path.canonicalize()? != path {
        return Err(io::Error::other(
            "temporary directory path is not canonical",
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
    let effective_uid = effective_uid();
    if !metadata.is_dir()
        || metadata.uid() != effective_uid
        || FileIdentity::from_metadata(&metadata) != expected
    {
        return Err(io::Error::other("temporary directory identity changed"));
    }
    let marker_path = path.join(OWNED_MARKER);
    let marker_metadata = fs::symlink_metadata(&marker_path)?;
    if !marker_metadata.is_file()
        || marker_metadata.uid() != effective_uid
        || fs::read_to_string(marker_path)?.trim() != expected.to_string()
    {
        return Err(io::Error::other("temporary directory marker is invalid"));
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory-safety preconditions.
    unsafe { libc::geteuid() }
}

pub fn validate_sysfs_geometry(sys_root: &Path, id: DeviceId) -> io::Result<()> {
    let device = sys_root.join("block").join(format!("ublkb{id}"));
    let logical = read_u64(&device.join("queue/logical_block_size"))?;
    let sectors = read_u64(&device.join("size"))?;
    let expected = Geometry::expected();
    if logical != expected.block_bytes || sectors != expected.sectors() {
        return Err(io::Error::other(format!(
            "ublkb{id} geometry is {logical}-byte blocks/{sectors} sectors, expected {}/{}",
            expected.block_bytes,
            expected.sectors()
        )));
    }
    Ok(())
}

pub fn validate_sysfs_device_identity(sys_root: &Path, id: DeviceId) -> io::Result<FileIdentity> {
    validate_sysfs_geometry(sys_root, id)?;
    let metadata = fs::metadata(sys_root.join("class/ublk-char").join(format!("ublkc{id}")))?;
    if !metadata.is_dir() {
        return Err(io::Error::other(
            "ublk sysfs class entry is not a directory",
        ));
    }
    Ok(FileIdentity::from_metadata(&metadata))
}

pub fn validate_device_identity(
    dev_root: &Path,
    sys_root: &Path,
    device: &DevicePath,
) -> io::Result<FileIdentity> {
    let id = device.id();
    let metadata = fs::metadata(dev_root.join(format!("ublkb{id}")))?;
    if !metadata.file_type().is_block_device() {
        return Err(io::Error::other("ublk device node is not a block device"));
    }
    let expected = read_device_number(
        &sys_root
            .join("block")
            .join(format!("ublkb{id}"))
            .join("dev"),
    )?;
    let actual = metadata.rdev();
    let actual = (libc::major(actual), libc::minor(actual));
    if actual != expected {
        return Err(io::Error::other(format!(
            "ublkb{id} node identity is {}:{}, expected {}:{}",
            actual.0, actual.1, expected.0, expected.1
        )));
    }
    validate_sysfs_device_identity(sys_root, id)
}

fn read_u64(path: &Path) -> io::Result<u64> {
    parse_decimal(&fs::read_to_string(path)?)
}

fn parse_decimal(value: &str) -> io::Result<u64> {
    let value = value.strip_suffix('\n').unwrap_or(value);
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected canonical unsigned decimal",
        ));
    }
    value
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_device_number(path: &Path) -> io::Result<(u32, u32)> {
    let value = fs::read_to_string(path)?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    let (major, minor) = value
        .split_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "expected major:minor"))?;
    let major = parse_decimal(major)?;
    let minor = parse_decimal(minor)?;
    Ok((
        major.try_into().map_err(io::Error::other)?,
        minor.try_into().map_err(io::Error::other)?,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FioRw {
    Write,
    RandomWrite,
}

impl fmt::Display for FioRw {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(match self {
            Self::Write => "write",
            Self::RandomWrite => "randwrite",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FioOptions {
    pub do_verify: Option<bool>,
    pub verify_only: bool,
    pub fsync: Option<u64>,
    pub output_json: bool,
}

pub fn fio_command(
    target: &Path,
    name: &str,
    pattern: u32,
    rw: FioRw,
    size: u64,
    offset: u64,
    options: FioOptions,
) -> Command {
    let mut command = Command::new("fio");
    command.args([
        format!("--name={name}"),
        format!("--filename={}", target.display()),
        "--direct=1".to_owned(),
        "--ioengine=sync".to_owned(),
        format!("--bs={BLOCK_BYTES}"),
        "--iodepth=1".to_owned(),
        "--numjobs=1".to_owned(),
        format!("--rw={rw}"),
        format!("--offset={offset}"),
        format!("--size={size}"),
        "--verify=md5".to_owned(),
        format!("--verify_pattern={pattern:#010x}"),
        "--verify_fatal=1".to_owned(),
        "--verify_dump=0".to_owned(),
        "--verify_state_save=0".to_owned(),
        "--randrepeat=1".to_owned(),
        "--randseed=74703".to_owned(),
        "--end_fsync=1".to_owned(),
    ]);
    if let Some(do_verify) = options.do_verify {
        command.arg(format!("--do_verify={}", u8::from(do_verify)));
    }
    if options.verify_only {
        command.arg("--verify_only=1");
    }
    if let Some(fsync) = options.fsync {
        command.arg(format!("--fsync={fsync}"));
    }
    if options.output_json {
        command.arg("--output-format=json");
    }
    command
}

pub fn validate_fio_command(command: &Command) -> io::Result<()> {
    if command.get_program() != "fio" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected an fio command",
        ));
    }
    let status = Command::new("fio")
        .args(["--parse-only", "--warnings-fatal"])
        .args(command.get_args())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "fio rejected command with {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "block-storage-lab-ublk-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn parses_only_exact_canonical_device_paths() {
        let path: DevicePath = "/dev/ublkb42".parse().unwrap();
        assert_eq!(path.id().get(), 42);
        assert_eq!(path.as_path(), Path::new("/dev/ublkb42"));
        for invalid in [
            "/dev/ublkb01",
            "/dev/ublkb",
            "/dev/ublkb1 extra",
            "/dev/sda",
        ] {
            assert!(invalid.parse::<DevicePath>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn expected_geometry_matches_the_ublk_acceptance_shape() {
        let geometry = Geometry::expected();
        assert_eq!(geometry.log_start_blocks, 6);
        assert_eq!(geometry.backing_blocks, 70);
        assert_eq!(geometry.volume_bytes(), 262_144);
        assert_eq!(geometry.backing_bytes(), 286_720);
        assert_eq!(geometry.sectors(), 512);
    }

    #[test]
    fn validates_parameterized_sysfs_geometry_and_identity() {
        let root = test_root("sysfs");
        let device = root.join("block/ublkb7");
        fs::create_dir_all(device.join("queue")).unwrap();
        fs::create_dir_all(root.join("class/ublk-char/ublkc7")).unwrap();
        fs::write(device.join("queue/logical_block_size"), "4096\n").unwrap();
        fs::write(device.join("size"), "512\n").unwrap();
        fs::write(device.join("dev"), "259:7\n").unwrap();
        let id = "7".parse().unwrap();
        let identity = validate_sysfs_device_identity(&root, id).unwrap();
        let dev_root = root.join("dev");
        fs::create_dir(&dev_root).unwrap();
        fs::write(dev_root.join("ublkb7"), "not a block node").unwrap();
        let device_path = "/dev/ublkb7".parse().unwrap();
        assert!(validate_device_identity(&dev_root, &root, &device_path).is_err());
        assert_eq!(
            identity,
            FileIdentity::from_metadata(
                &fs::metadata(root.join("class/ublk-char/ublkc7")).unwrap()
            )
        );
        assert_eq!(read_device_number(&device.join("dev")).unwrap(), (259, 7));
        fs::write(device.join("dev"), "0259:7\n").unwrap();
        assert!(read_device_number(&device.join("dev")).is_err());
        fs::write(device.join("size"), "4096\n").unwrap();
        assert!(validate_sysfs_geometry(&root, id).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owned_directory_requires_its_original_identity_and_marker() {
        let root = test_root("owned");
        let mut owned = OwnedTempDir::create(&root).unwrap();
        assert!(owned.path().starts_with(&root));
        assert_eq!(
            FileIdentity::from_metadata(&fs::metadata(owned.path()).unwrap()),
            owned.identity()
        );
        owned.validate().unwrap();
        fs::write(owned.path().join("backing.img"), "owned").unwrap();
        fs::write(owned.path().join(OWNED_MARKER), "wrong\n").unwrap();
        assert!(owned.cleanup().is_err());
        fs::write(
            owned.path().join(OWNED_MARKER),
            format!("{}\n", owned.identity()),
        )
        .unwrap();
        owned.cleanup().unwrap();
        assert!(!owned.path().exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn fio_command_never_saves_verify_state_in_the_caller_directory() {
        let command = fio_command(
            Path::new("/dev/ublkb7"),
            "verify-state-regression",
            0x1357_9bdf,
            FioRw::Write,
            BLOCK_BYTES,
            0,
            FioOptions {
                verify_only: true,
                ..FioOptions::default()
            },
        );
        let args = command
            .get_args()
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>();

        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("--verify_state_save="))
                .count(),
            1
        );
        assert!(args.iter().any(|arg| arg == "--verify_state_save=0"));
    }

    #[test]
    fn installed_fio_accepts_constructed_commands() {
        let target = Path::new("/nonexistent/my-block-storage-fio-target");
        let cases = [
            (
                "sequential",
                FioRw::Write,
                FioOptions {
                    do_verify: Some(true),
                    fsync: Some(16),
                    ..FioOptions::default()
                },
            ),
            (
                "random",
                FioRw::RandomWrite,
                FioOptions {
                    do_verify: Some(true),
                    fsync: Some(16),
                    ..FioOptions::default()
                },
            ),
            (
                "restart",
                FioRw::Write,
                FioOptions {
                    verify_only: true,
                    ..FioOptions::default()
                },
            ),
            (
                "exhaustion",
                FioRw::Write,
                FioOptions {
                    do_verify: Some(false),
                    fsync: Some(32),
                    output_json: true,
                    ..FioOptions::default()
                },
            ),
        ];
        for (name, rw, options) in cases {
            let command = fio_command(target, name, 0x1357_9bdf, rw, 16 * BLOCK_BYTES, 0, options);
            validate_fio_command(&command).unwrap();
        }
    }
}
