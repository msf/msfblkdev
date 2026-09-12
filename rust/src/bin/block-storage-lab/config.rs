use std::ffi::OsString;
use std::time::{Duration, Instant};

pub const SLOW_ENV: &str = "TEST_SLOW_SECONDS";
pub const PER_TEST_ENV: &str = "TEST_PER_TEST_SECONDS";
pub const SUITE_ENV: &str = "TEST_SUITE_SECONDS";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Test,
    EngineCrash,
    UblkFio,
    Ext4,
    Ext4Preflight,
    Ext4Populate,
    Ext4Verify,
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
        _ => Err(usage(&program)),
    }
}

pub fn from_env<F>(mode: Mode, mut get: F) -> Result<Config, String>
where
    F: FnMut(&str) -> Option<OsString>,
{
    let (default_per_test, default_suite) = match mode {
        Mode::Test => (15, 55),
        Mode::EngineCrash => (1800, 3600),
        Mode::UblkFio => (30, 180),
        Mode::Ext4 | Mode::Ext4Preflight | Mode::Ext4Populate | Mode::Ext4Verify => (60, 600),
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
        "usage: {} test | engine-crash | ublk-fio | ext4 | ext4-preflight",
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
        for args in [
            vec!["lab"],
            vec!["lab", "unknown"],
            vec!["lab", "test", "extra"],
            vec!["lab", "ext4", "/dev/sda"],
            vec!["lab", "ext4", "/tmp/mount"],
            vec!["lab", "ext4-populate", "/tmp/mount"],
        ] {
            assert!(parse_args(args.into_iter().map(OsString::from)).is_err());
        }
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
