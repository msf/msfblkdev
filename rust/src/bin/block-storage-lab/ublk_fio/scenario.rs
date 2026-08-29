use super::*;

pub(super) const REPETITIONS: u8 = 3;
pub(super) const FAILPOINT: &str = "descriptor-write-complete";
#[cfg(test)]
const RANDOM_SEED: u32 = 74_703;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Scenario {
    Sequential,
    Random,
    Overwrite,
    GracefulRestart,
    SigkillRestart,
    DescriptorCloseFailpoint,
    Exhaustion,
}

pub(super) const SCENARIOS: [Scenario; 7] = [
    Scenario::Sequential,
    Scenario::Random,
    Scenario::Overwrite,
    Scenario::GracefulRestart,
    Scenario::SigkillRestart,
    Scenario::DescriptorCloseFailpoint,
    Scenario::Exhaustion,
];

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Transition {
    Start,
    Workload,
    GracefulStop,
    Sigkill,
    WaitForFailpoint,
    RemoveOldDevice,
    Restart,
    Verify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FioJob {
    pub name: &'static str,
    pub pattern: u32,
    pub rw: FioRw,
    pub blocks: u64,
    pub options: FioOptions,
}

impl FioJob {
    pub fn command(self, target: &Path) -> Command {
        ublk::fio_command(
            target,
            self.name,
            self.pattern,
            self.rw,
            self.blocks * BLOCK_BYTES,
            0,
            self.options,
        )
    }
}

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Random => "random",
            Self::Overwrite => "overwrite",
            Self::GracefulRestart => "graceful-restart",
            Self::SigkillRestart => "sigkill-restart",
            Self::DescriptorCloseFailpoint => "descriptor-close-failpoint",
            Self::Exhaustion => "exhaustion",
        }
    }

    #[cfg(test)]
    pub fn attempted_writes(self) -> u64 {
        match self {
            Self::Sequential | Self::Random => 16,
            Self::Overwrite
            | Self::GracefulRestart
            | Self::SigkillRestart
            | Self::DescriptorCloseFailpoint => 8,
            Self::Exhaustion => 33,
        }
    }

    #[cfg(test)]
    pub fn successful_writes(self) -> u64 {
        self.attempted_writes().min(RECORD_CAPACITY)
    }

    #[cfg(test)]
    pub fn transitions(self) -> &'static [Transition] {
        use Transition::*;
        match self {
            Self::Sequential | Self::Random | Self::Overwrite => &[Start, Workload],
            Self::GracefulRestart | Self::Exhaustion => {
                &[Start, Workload, GracefulStop, Restart, Verify]
            }
            Self::SigkillRestart => &[Start, Workload, Sigkill, RemoveOldDevice, Restart, Verify],
            Self::DescriptorCloseFailpoint => &[
                Start,
                Workload,
                GracefulStop,
                WaitForFailpoint,
                Sigkill,
                RemoveOldDevice,
                Restart,
                Verify,
            ],
        }
    }

    pub fn fio_jobs(self) -> Vec<FioJob> {
        match self {
            Self::Sequential => vec![write_verify("sequential", 0x1357_9bdf, FioRw::Write, 16)],
            Self::Random => vec![write_verify("random", 0x2468_ace0, FioRw::RandomWrite, 16)],
            Self::Overwrite => (1..=8)
                .map(|byte| {
                    let pattern = u32::from_ne_bytes([byte; 4]);
                    write_verify("overwrite", pattern, FioRw::Write, 1)
                })
                .collect(),
            Self::GracefulRestart => restart_jobs("graceful", 0x1122_3344, 8),
            Self::SigkillRestart => restart_jobs("sigkill", 0x5566_7788, 8),
            Self::DescriptorCloseFailpoint => restart_jobs("close-failpoint", 0x99aa_bbcc, 8),
            Self::Exhaustion => vec![
                FioJob {
                    name: "exhaustion",
                    pattern: 0xdead_beef,
                    rw: FioRw::Write,
                    blocks: 33,
                    options: FioOptions {
                        do_verify: Some(false),
                        fsync: Some(32),
                        output_json: true,
                        ..FioOptions::default()
                    },
                },
                verify("exhaustion", 0xdead_beef, 32),
            ],
        }
    }
}

fn write_verify(name: &'static str, pattern: u32, rw: FioRw, blocks: u64) -> FioJob {
    FioJob {
        name,
        pattern,
        rw,
        blocks,
        options: FioOptions {
            do_verify: Some(true),
            fsync: Some(blocks),
            ..FioOptions::default()
        },
    }
}

fn restart_jobs(name: &'static str, pattern: u32, blocks: u64) -> Vec<FioJob> {
    vec![
        FioJob {
            name,
            pattern,
            rw: FioRw::Write,
            blocks,
            options: FioOptions {
                do_verify: Some(false),
                fsync: Some(blocks),
                ..FioOptions::default()
            },
        },
        verify(name, pattern, blocks),
    ]
}

fn verify(name: &'static str, pattern: u32, blocks: u64) -> FioJob {
    FioJob {
        name,
        pattern,
        rw: FioRw::Write,
        blocks,
        options: FioOptions {
            verify_only: true,
            ..FioOptions::default()
        },
    }
}

pub(super) fn all_fio_commands(target: &Path) -> Vec<Command> {
    SCENARIOS
        .iter()
        .flat_map(|scenario| scenario.fio_jobs())
        .map(|job| job.command(target))
        .collect()
}

#[derive(Default)]
pub(super) struct OutputNames {
    daemon: u32,
    fio: u32,
    command: u32,
}

impl OutputNames {
    pub fn daemon(&mut self) -> (String, String) {
        let sequence = self.daemon;
        self.daemon += 1;
        output_pair("daemon", sequence)
    }

    pub fn fio(&mut self) -> (String, String) {
        let sequence = self.fio;
        self.fio += 1;
        output_pair("fio", sequence)
    }

    pub fn command(&mut self, label: &str) -> (String, String) {
        let sequence = self.command;
        self.command += 1;
        output_pair(label, sequence)
    }
}

fn output_pair(label: &str, sequence: u32) -> (String, String) {
    (
        format!("{label}.{sequence}.stdout"),
        format!("{label}.{sequence}.stderr"),
    )
}

pub(super) fn parse_exhaustion_json(input: &str) -> io::Result<(u64, u64)> {
    let jobs = object_after_key(input, "jobs")?;
    let job = jobs
        .strip_prefix('[')
        .and_then(|value| value.trim_start().strip_prefix('{'))
        .ok_or_else(|| invalid_json("jobs must contain an object"))?;
    let error = number_after_key(job, "error")?;
    let write = object_after_key(job, "write")?;
    let io_bytes = number_after_key(write, "io_bytes")?;
    Ok((error, io_bytes))
}

fn object_after_key<'a>(input: &'a str, key: &str) -> io::Result<&'a str> {
    value_after_key(input, key).map(str::trim_start)
}

fn number_after_key(input: &str, key: &str) -> io::Result<u64> {
    let value = value_after_key(input, key)?.trim_start();
    let end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let valid_delimiter = value[end..]
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_whitespace() || matches!(character, ',' | '}'));
    if end == 0 || !valid_delimiter {
        return Err(invalid_json("invalid numeric fio field"));
    }
    value[..end]
        .parse()
        .map_err(|_| invalid_json("invalid fio number"))
}

fn value_after_key<'a>(input: &'a str, key: &str) -> io::Result<&'a str> {
    let needle = format!("\"{key}\"");
    let mut remaining = input;
    while let Some((_, rest)) = remaining.split_once(&needle) {
        let rest = rest.trim_start();
        if let Some(value) = rest.strip_prefix(':') {
            return Ok(value);
        }
        remaining = rest;
    }
    Err(invalid_json("required fio field is missing"))
}

fn invalid_json(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_count_repetitions_and_write_budgets_are_exact() {
        assert_eq!(SCENARIOS.len(), 7);
        assert_eq!(REPETITIONS, 3);
        assert_eq!(
            SCENARIOS
                .iter()
                .map(|s| s.attempted_writes())
                .collect::<Vec<_>>(),
            [16, 16, 8, 8, 8, 8, 33]
        );
        assert!(SCENARIOS[..6].iter().all(|s| s.successful_writes() <= 16));
        assert_eq!(Scenario::Exhaustion.successful_writes(), 32);
    }

    #[test]
    fn restart_and_fault_transitions_are_explicit() {
        assert_eq!(
            Scenario::GracefulRestart.transitions(),
            &[
                Transition::Start,
                Transition::Workload,
                Transition::GracefulStop,
                Transition::Restart,
                Transition::Verify
            ]
        );
        assert!(
            Scenario::SigkillRestart
                .transitions()
                .contains(&Transition::RemoveOldDevice)
        );
        assert_eq!(
            Scenario::DescriptorCloseFailpoint.transitions()[3],
            Transition::WaitForFailpoint
        );
    }

    #[test]
    fn output_names_never_reuse_owned_paths() {
        let mut names = OutputNames::default();
        assert_eq!(
            names.daemon(),
            ("daemon.0.stdout".into(), "daemon.0.stderr".into())
        );
        assert_eq!(
            names.daemon(),
            ("daemon.1.stdout".into(), "daemon.1.stderr".into())
        );
        assert_eq!(names.fio(), ("fio.0.stdout".into(), "fio.0.stderr".into()));
        assert_eq!(
            names.command("delete"),
            ("delete.0.stdout".into(), "delete.0.stderr".into())
        );
    }

    #[test]
    fn parses_only_required_fio_numeric_fields() {
        let json = r#"{"jobs":[{"job options":{"rw":"write"},"error" : 28, "read":{"io_bytes":0}, "write" : {"io_bytes" : 131072}}]}"#;
        assert_eq!(parse_exhaustion_json(json).unwrap(), (28, 131_072));
        assert!(
            parse_exhaustion_json(r#"{"jobs":[{"error":"28","write":{"io_bytes":131072}}]}"#)
                .is_err()
        );
        assert!(parse_exhaustion_json(r#"{"jobs":[]}"#).is_err());
        assert!(
            parse_exhaustion_json(r#"{"jobs":[{"error":28oops,"write":{"io_bytes":131072}}]}"#)
                .is_err()
        );
    }

    #[test]
    fn every_exact_command_shape_has_expected_budget_and_seed() {
        let commands = all_fio_commands(Path::new("/nonexistent/fio-target"));
        assert_eq!(commands.len(), 18);
        for command in commands {
            let args = command
                .get_args()
                .map(OsStr::to_string_lossy)
                .collect::<Vec<_>>();
            assert!(args.contains(&format!("--randseed={RANDOM_SEED}").into()));
            assert!(args.contains(&"--direct=1".into()));
            assert!(args.contains(&"--bs=4096".into()));
        }
    }
}
