use super::*;

pub(super) const REPETITIONS: u8 = 3;
pub(super) const FAILPOINT: &str = "descriptor-write-complete";
#[cfg(test)]
const RANDOM_SEED: u32 = 74_703;

/// Full-region overwrite passes in the wraparound scenario. Each write-through pass appends two
/// blocks per written block, so the passes append several times the log capacity.
pub(super) const WRAP_PASSES: u32 = 12;
/// The fewest blocks the thin exhaustion write must land before `ENOSPC`: the space foreground
/// writes may use without any compaction is five of the ten segments (two open heads and a
/// three-segment reserve are excluded), 31 writable blocks each, so 77 write-through records of
/// two blocks. Compaction under pressure lands more (102–190 observed).
pub(super) const EXHAUSTION_MINIMUM_BLOCKS: u64 = 77;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Scenario {
    Sequential,
    Random,
    Overwrite,
    GracefulRestart,
    SigkillRestart,
    DescriptorCloseFailpoint,
    /// Distinct blocks beyond the log's capacity: `ENOSPC`, then the written prefix survives.
    Exhaustion,
    /// Overwrites the same region until the log has wrapped several times, then restarts
    /// gracefully and by SIGKILL; the last pass must verify after each restart.
    SustainedOverwrite,
}

pub(super) const SCENARIOS: [Scenario; 8] = [
    Scenario::Sequential,
    Scenario::Random,
    Scenario::Overwrite,
    Scenario::GracefulRestart,
    Scenario::SigkillRestart,
    Scenario::DescriptorCloseFailpoint,
    Scenario::Exhaustion,
    Scenario::SustainedOverwrite,
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
            Self::SustainedOverwrite => "sustained-overwrite",
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
            Self::Exhaustion => Geometry::expected().volume_blocks,
            Self::SustainedOverwrite => u64::from(WRAP_PASSES) * WRAP_REGION_BLOCKS,
        }
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
            Self::SustainedOverwrite => &[
                Start,
                Workload,
                GracefulStop,
                Restart,
                Verify,
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
            Self::Exhaustion => vec![FioJob {
                name: "exhaustion",
                pattern: 0xdead_beef,
                rw: FioRw::Write,
                // The thin volume holds more distinct blocks than the log can ever store,
                // so this write must end in ENOSPC whatever the cleaner compacts. One flush
                // per write makes every write its own record. The verify job is built at
                // run time from the bytes fio reports as written.
                blocks: Geometry::expected().volume_blocks,
                options: FioOptions {
                    do_verify: Some(false),
                    fsync: Some(1),
                    output_json: true,
                    ..FioOptions::default()
                },
            }],
            Self::SustainedOverwrite => wrap_jobs(),
        }
    }
}

/// The wraparound passes: each rewrites the whole region with its own pattern, write-through,
/// and verifies it; the final verify jobs re-check the last pass after each restart.
fn wrap_jobs() -> Vec<FioJob> {
    let mut jobs: Vec<FioJob> = (1..=WRAP_PASSES)
        .map(|pass| {
            let pattern = 0x5750_0000 | pass;
            FioJob {
                name: "sustained-overwrite",
                pattern,
                rw: FioRw::Write,
                blocks: WRAP_REGION_BLOCKS,
                options: FioOptions {
                    do_verify: Some(true),
                    fsync: Some(1),
                    ..FioOptions::default()
                },
            }
        })
        .collect();
    let last = jobs.last().expect("at least one pass").pattern;
    jobs.push(verify("sustained-overwrite", last, WRAP_REGION_BLOCKS));
    jobs
}

/// The verify job for whatever prefix the exhaustion write landed.
pub(super) fn exhaustion_verify(blocks: u64) -> FioJob {
    verify("exhaustion", 0xdead_beef, blocks)
}

/// Blocks the wraparound passes append at least: two per write-through record.
pub(super) fn wrap_appended_blocks() -> u64 {
    2 * u64::from(WRAP_PASSES) * WRAP_REGION_BLOCKS
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

/// Reads `name=NUMBER` from the daemon's structured log lines.
pub(super) fn statistic(text: &str, name: &str) -> io::Result<u64> {
    let key = format!(" {name}=");
    let position = text
        .rfind(&key)
        .ok_or_else(|| io::Error::other(format!("daemon log lacks {name}")))?;
    let digits: String = text[position + key.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .map_err(|_| io::Error::other(format!("daemon log has no number for {name}")))
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
        assert_eq!(SCENARIOS.len(), 8);
        assert_eq!(REPETITIONS, 3);
        assert_eq!(
            SCENARIOS
                .iter()
                .map(|s| s.attempted_writes())
                .collect::<Vec<_>>(),
            [16, 16, 8, 8, 8, 8, 1024, 768]
        );
        let geometry = Geometry::expected();
        // Exhaustion must exceed the log even when perfectly compacted; the wraparound passes
        // must append more than three times the log capacity.
        assert!(Scenario::Exhaustion.attempted_writes() > geometry.log_capacity_blocks());
        assert!(EXHAUSTION_MINIMUM_BLOCKS < geometry.log_capacity_blocks());
        assert!(wrap_appended_blocks() >= 3 * geometry.log_capacity_blocks());
        assert_eq!(exhaustion_verify(190).blocks, 190);
        assert!(exhaustion_verify(190).options.verify_only);
        let wrap = Scenario::SustainedOverwrite.fio_jobs();
        assert_eq!(wrap.len(), WRAP_PASSES as usize + 1);
        assert!(
            wrap[..WRAP_PASSES as usize]
                .iter()
                .all(|job| job.options.fsync == Some(1))
        );
        assert_eq!(
            wrap[WRAP_PASSES as usize].pattern,
            wrap[WRAP_PASSES as usize - 1].pattern
        );
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
        let wrap = Scenario::SustainedOverwrite.transitions();
        assert_eq!(
            wrap.iter().filter(|t| **t == Transition::Restart).count(),
            2
        );
        assert_eq!(wrap.iter().filter(|t| **t == Transition::Verify).count(), 2);
        assert!(wrap.contains(&Transition::Sigkill));
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
    fn statistics_are_read_from_the_last_structured_log_line() {
        let text = "event=reclaim_stats reclaim_segments_freed=3 reclaim_pressure_steps=9\nevent=log_writes log_link_blocks=12\nevent=reclaim_stats reclaim_segments_freed=17 x=1\n";
        assert_eq!(statistic(text, "reclaim_segments_freed").unwrap(), 17);
        assert_eq!(statistic(text, "log_link_blocks").unwrap(), 12);
        assert!(statistic(text, "segments_freed").is_err());
        assert!(statistic("event=x reclaim_segments_freed=", "reclaim_segments_freed").is_err());
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
        assert_eq!(commands.len(), 17 + WRAP_PASSES as usize + 1);
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
