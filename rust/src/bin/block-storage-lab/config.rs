use std::ffi::{OsStr, OsString};
use std::time::{Duration, Instant};

pub const SLOW_ENV: &str = "TEST_SLOW_SECONDS";
pub const PER_TEST_ENV: &str = "TEST_PER_TEST_SECONDS";
pub const SUITE_ENV: &str = "TEST_SUITE_SECONDS";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseSnapshotPhase {
    DrainStart,
    DrainEnd,
}

impl DatabaseSnapshotPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DrainStart => "drain_start",
            Self::DrainEnd => "drain_end",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseSnapshotRequest {
    pub phase: DatabaseSnapshotPhase,
    pub connection: u8,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SqliteProfile {
    DeleteExtra,
    WalFull,
    WalNormal,
}

impl SqliteProfile {
    pub const fn name(self) -> &'static str {
        match self {
            Self::DeleteExtra => "delete-extra",
            Self::WalFull => "wal-full",
            Self::WalNormal => "wal-normal",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "delete-extra" => Some(Self::DeleteExtra),
            "wal-full" => Some(Self::WalFull),
            "wal-normal" => Some(Self::WalNormal),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SqliteCase {
    ProbeSeededRandomUpdate,
    SpeedtestMain,
    KvSequentialReadSmall,
    KvRandomReadSmall,
    KvSequentialReadLarge,
    KvRandomReadLarge,
    KvSequentialUpdateSmallSingleTransaction,
    KvSequentialUpdateSmallPerOpCommit,
    KvRandomUpdateLargeSingleTransaction,
    KvRandomUpdateLargePerOpCommit,
    KvLongWalCheckpointStress,
}

impl SqliteCase {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ProbeSeededRandomUpdate => "kv-seeded-random-update",
            Self::SpeedtestMain => "speedtest-main",
            Self::KvSequentialReadSmall => "kv-sequential-read-small",
            Self::KvRandomReadSmall => "kv-random-read-small",
            Self::KvSequentialReadLarge => "kv-sequential-read-large",
            Self::KvRandomReadLarge => "kv-random-read-large",
            Self::KvSequentialUpdateSmallSingleTransaction => {
                "kv-sequential-update-small-single-transaction"
            }
            Self::KvSequentialUpdateSmallPerOpCommit => "kv-sequential-update-small-per-op-commit",
            Self::KvRandomUpdateLargeSingleTransaction => {
                "kv-random-update-large-single-transaction"
            }
            Self::KvRandomUpdateLargePerOpCommit => "kv-random-update-large-per-op-commit",
            Self::KvLongWalCheckpointStress => "kv-long-wal-checkpoint-stress",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        SQLITE_MATRIX_CASES
            .into_iter()
            .find(|case| case.name() == name)
    }
}

pub const SQLITE_MATRIX_CASES: [SqliteCase; 10] = [
    SqliteCase::SpeedtestMain,
    SqliteCase::KvSequentialReadSmall,
    SqliteCase::KvRandomReadSmall,
    SqliteCase::KvSequentialReadLarge,
    SqliteCase::KvRandomReadLarge,
    SqliteCase::KvSequentialUpdateSmallSingleTransaction,
    SqliteCase::KvSequentialUpdateSmallPerOpCommit,
    SqliteCase::KvRandomUpdateLargeSingleTransaction,
    SqliteCase::KvRandomUpdateLargePerOpCommit,
    SqliteCase::KvLongWalCheckpointStress,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteSelector {
    pub case: SqliteCase,
    pub profile: SqliteProfile,
    pub repetition: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Test,
    EngineCrash,
    UblkFio,
    Ext4,
    Ext4Preflight,
    Ext4Populate,
    Ext4Verify,
    Xfs,
    XfsPreflight,
    XfsPopulate,
    XfsVerify,
    SqliteProbe,
    SqliteExt4(Option<SqliteSelector>),
    DatabaseSnapshot(DatabaseSnapshotRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub mode: Mode,
    pub slow: Duration,
    pub per_test: Duration,
    pub suite: Duration,
}

pub fn parse_args<I>(args: I) -> Result<Mode, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("block-storage-lab"));
    let subcommand = args.next();
    if subcommand.as_deref() == Some(OsStr::new("database-snapshot")) {
        return parse_database_snapshot(&program, &mut args);
    }
    if subcommand.as_deref() == Some(OsStr::new("sqlite-ext4")) {
        return parse_sqlite_ext4(&program, &mut args);
    }
    if args.next().is_some() {
        return Err(usage(&program));
    }
    match subcommand.as_deref().and_then(|value| value.to_str()) {
        Some("test") => Ok(Mode::Test),
        Some("engine-crash") => Ok(Mode::EngineCrash),
        Some("ublk-fio") => Ok(Mode::UblkFio),
        Some("ext4") => Ok(Mode::Ext4),
        Some("ext4-preflight") => Ok(Mode::Ext4Preflight),
        Some("ext4-populate") => Ok(Mode::Ext4Populate),
        Some("ext4-verify") => Ok(Mode::Ext4Verify),
        Some("xfs") => Ok(Mode::Xfs),
        Some("xfs-preflight") => Ok(Mode::XfsPreflight),
        Some("xfs-populate") => Ok(Mode::XfsPopulate),
        Some("xfs-verify") => Ok(Mode::XfsVerify),
        Some("sqlite-probe") => Ok(Mode::SqliteProbe),
        _ => Err(usage(&program)),
    }
}

fn parse_sqlite_ext4<I>(program: &OsString, args: &mut I) -> Result<Mode, String>
where
    I: Iterator<Item = OsString>,
{
    let Some(case) = args.next() else {
        return Ok(Mode::SqliteExt4(None));
    };
    let profile = args.next().ok_or_else(|| usage(program))?;
    let repetition = args.next().ok_or_else(|| usage(program))?;
    if args.next().is_some() {
        return Err(usage(program));
    }
    let case = case
        .to_str()
        .and_then(SqliteCase::parse)
        .ok_or_else(|| usage(program))?;
    let profile = profile
        .to_str()
        .and_then(SqliteProfile::parse)
        .ok_or_else(|| usage(program))?;
    let repetition = repetition
        .to_str()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| (1..=3).contains(value))
        .ok_or_else(|| usage(program))?;
    if case == SqliteCase::KvLongWalCheckpointStress && profile == SqliteProfile::DeleteExtra {
        return Err(usage(program));
    }
    Ok(Mode::SqliteExt4(Some(SqliteSelector {
        case,
        profile,
        repetition,
    })))
}

fn parse_database_snapshot<I>(program: &OsString, args: &mut I) -> Result<Mode, String>
where
    I: Iterator<Item = OsString>,
{
    let phase = match args.next().as_deref().and_then(OsStr::to_str) {
        Some("drain-start") => DatabaseSnapshotPhase::DrainStart,
        Some("drain-end") => DatabaseSnapshotPhase::DrainEnd,
        _ => return Err(usage(program)),
    };
    let Some(connection) = args.next() else {
        return Err(usage(program));
    };
    if args.next().is_some() {
        return Err(usage(program));
    }
    let text = connection.to_str().ok_or_else(|| usage(program))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(usage(program));
    }
    let connection = text.parse::<u8>().map_err(|_| usage(program))?;
    if !(1..=64).contains(&connection) {
        return Err(usage(program));
    }
    Ok(Mode::DatabaseSnapshot(DatabaseSnapshotRequest {
        phase,
        connection,
    }))
}

pub fn from_env<F>(mode: Mode, mut get: F) -> Result<Config, String>
where
    F: FnMut(&str) -> Option<OsString>,
{
    let (default_per_test, default_suite) = match mode {
        Mode::Test => (15, 55),
        Mode::EngineCrash => (1800, 3600),
        Mode::UblkFio => (30, 180),
        Mode::Ext4
        | Mode::Ext4Preflight
        | Mode::Ext4Populate
        | Mode::Ext4Verify
        | Mode::Xfs
        | Mode::XfsPreflight
        | Mode::XfsPopulate
        | Mode::XfsVerify => (60, 600),
        Mode::SqliteProbe => (60, 600),
        Mode::SqliteExt4(_) => (60, 600),
        Mode::DatabaseSnapshot(_) => (1, 1),
    };
    Ok(Config {
        mode,
        slow: seconds_from_env(SLOW_ENV, 1, true, &mut get)?,
        per_test: seconds_from_env(PER_TEST_ENV, default_per_test, false, &mut get)?,
        suite: seconds_from_env(SUITE_ENV, default_suite, false, &mut get)?,
    })
}

fn seconds_from_env<F>(
    name: &str,
    default: u64,
    allow_zero: bool,
    get: &mut F,
) -> Result<Duration, String>
where
    F: FnMut(&str) -> Option<OsString>,
{
    let Some(raw) = get(name) else {
        return Ok(Duration::from_secs(default));
    };
    let text = raw
        .to_str()
        .ok_or_else(|| invalid_env(name, &raw, allow_zero))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_env(name, &raw, allow_zero));
    }
    let value = text
        .parse::<u64>()
        .map_err(|_| invalid_env(name, &raw, allow_zero))?;
    if value == 0 && !allow_zero {
        return Err(invalid_env(name, &raw, allow_zero));
    }
    let duration = Duration::from_secs(value);
    if Instant::now().checked_add(duration).is_none() {
        return Err(invalid_env(name, &raw, allow_zero));
    }
    Ok(duration)
}

fn invalid_env(name: &str, value: &OsString, allow_zero: bool) -> String {
    let expected = if allow_zero {
        "non-negative"
    } else {
        "positive"
    };
    format!(
        "invalid {name}: {} (expected {expected} integer)",
        value.to_string_lossy()
    )
}

fn usage(program: &OsString) -> String {
    format!(
        "usage: {} test | engine-crash | ublk-fio | ext4 | ext4-preflight | xfs | xfs-preflight | sqlite-probe | sqlite-ext4 [<case> <profile> <repetition 1..3>] | database-snapshot drain-start|drain-end <connection-id>",
        program.to_string_lossy()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn validates_arguments() {
        assert_eq!(
            parse_args(["lab", "test"].map(OsString::from)),
            Ok(Mode::Test)
        );
        assert_eq!(
            parse_args(["lab", "engine-crash"].map(OsString::from)),
            Ok(Mode::EngineCrash)
        );
        assert_eq!(
            parse_args(["lab", "ublk-fio"].map(OsString::from)),
            Ok(Mode::UblkFio)
        );
        assert_eq!(
            parse_args(["lab", "xfs"].map(OsString::from)),
            Ok(Mode::Xfs)
        );
        assert_eq!(
            parse_args(["lab", "xfs-preflight"].map(OsString::from)),
            Ok(Mode::XfsPreflight)
        );
        assert_eq!(
            parse_args(["lab", "database-snapshot", "drain-start", "1"].map(OsString::from)),
            Ok(Mode::DatabaseSnapshot(DatabaseSnapshotRequest {
                phase: DatabaseSnapshotPhase::DrainStart,
                connection: 1,
            }))
        );
        assert_eq!(
            parse_args(["lab", "database-snapshot", "drain-end", "64"].map(OsString::from)),
            Ok(Mode::DatabaseSnapshot(DatabaseSnapshotRequest {
                phase: DatabaseSnapshotPhase::DrainEnd,
                connection: 64,
            }))
        );
        for args in [
            vec!["lab"],
            vec!["lab", "unknown"],
            vec!["lab", "test", "extra"],
            vec!["lab", "ext4", "/dev/sda"],
            vec!["lab", "ext4", "/tmp/mount"],
            vec!["lab", "ext4-populate", "/tmp/mount"],
            vec!["lab", "xfs", "/dev/sda"],
            vec!["lab", "xfs-preflight", "extra"],
            vec!["lab", "xfs-populate", "/tmp/mount"],
            vec!["lab", "xfs-verify", "/tmp/mount"],
            vec!["lab", "sqlite-probe", "/tmp/backing"],
            vec!["lab", "sqlite-probe", "/tmp/backing", "/tmp/mount"],
            vec!["lab", "database-snapshot"],
            vec!["lab", "database-snapshot", "bad", "1"],
            vec!["lab", "database-snapshot", "drain-start", "0"],
            vec!["lab", "database-snapshot", "drain-end", "65"],
            vec!["lab", "database-snapshot", "drain-end", "1", "/tmp/path"],
        ] {
            assert!(parse_args(args.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn validates_sqlite_ext4_arguments_and_defaults() {
        assert_eq!(
            parse_args(["lab", "sqlite-probe"].map(OsString::from)),
            Ok(Mode::SqliteProbe)
        );
        assert_eq!(
            parse_args(["lab", "sqlite-ext4"].map(OsString::from)),
            Ok(Mode::SqliteExt4(None))
        );
        assert_eq!(
            parse_args(
                ["lab", "sqlite-ext4", "speedtest-main", "wal-full", "2"].map(OsString::from)
            ),
            Ok(Mode::SqliteExt4(Some(SqliteSelector {
                case: SqliteCase::SpeedtestMain,
                profile: SqliteProfile::WalFull,
                repetition: 2,
            })))
        );
        for args in [
            ["lab", "sqlite-ext4", "unknown", "wal-full", "1"],
            ["lab", "sqlite-ext4", "speedtest-main", "bad", "1"],
            [
                "lab",
                "sqlite-ext4",
                "kv-long-wal-checkpoint-stress",
                "delete-extra",
                "1",
            ],
            ["lab", "sqlite-ext4", "speedtest-main", "wal-full", "0"],
            ["lab", "sqlite-ext4", "speedtest-main", "wal-full", "4"],
            ["lab", "sqlite-ext4", "speedtest-main", "wal-full", "x"],
        ] {
            assert!(parse_args(args.map(OsString::from)).is_err());
        }
        let probe = from_env(Mode::SqliteProbe, |_| None).unwrap();
        assert_eq!((probe.per_test.as_secs(), probe.suite.as_secs()), (60, 600));
        let matrix = from_env(Mode::SqliteExt4(None), |_| None).unwrap();
        assert_eq!(
            (matrix.per_test.as_secs(), matrix.suite.as_secs()),
            (60, 600)
        );
    }

    #[test]
    fn validates_environment_and_defaults() {
        let empty: HashMap<&str, OsString> = HashMap::new();
        let test = from_env(Mode::Test, |name| empty.get(name).cloned()).unwrap();
        assert_eq!(test.per_test, Duration::from_secs(15));
        assert_eq!(test.suite, Duration::from_secs(55));

        let ext4 = from_env(Mode::Ext4, |_| None).unwrap();
        assert_eq!(ext4.per_test, Duration::from_secs(60));
        assert_eq!(ext4.suite, Duration::from_secs(600));
        assert_eq!(
            parse_args(["lab", "ext4"].map(OsString::from)),
            Ok(Mode::Ext4)
        );

        for (command, mode) in [
            ("xfs", Mode::Xfs),
            ("xfs-preflight", Mode::XfsPreflight),
            ("xfs-populate", Mode::XfsPopulate),
            ("xfs-verify", Mode::XfsVerify),
        ] {
            assert_eq!(parse_args(["lab", command].map(OsString::from)), Ok(mode));
            let config = from_env(mode, |_| None).unwrap();
            assert_eq!(config.per_test, Duration::from_secs(60));
            assert_eq!(config.suite, Duration::from_secs(600));
        }

        let values = HashMap::from([
            (SLOW_ENV, OsString::from("0")),
            (PER_TEST_ENV, OsString::from("7")),
            (SUITE_ENV, OsString::from("8")),
        ]);
        let configured = from_env(Mode::EngineCrash, |name| values.get(name).cloned()).unwrap();
        assert_eq!(configured.slow, Duration::ZERO);
        assert_eq!(configured.per_test, Duration::from_secs(7));
        assert_eq!(configured.suite, Duration::from_secs(8));

        for (name, value) in [
            (SLOW_ENV, "-1"),
            (PER_TEST_ENV, "0"),
            (SUITE_ENV, "1.5"),
            (SUITE_ENV, ""),
            (SUITE_ENV, "18446744073709551615"),
        ] {
            let values = HashMap::from([(name, OsString::from(value))]);
            assert!(from_env(Mode::Test, |key| values.get(key).cloned()).is_err());
        }
    }
}
