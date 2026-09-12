use crate::config::{Config, Mode};
use crate::evidence::Evidence;
use crate::process::{self, Outcome};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CRASH_MODE_ENV: &str = "BLOCK_STORAGE_CRASH_TEST_MODE";

#[derive(Clone, Copy)]
struct TestGroup {
    label: &'static str,
    target: &'static str,
}

const GROUPS: [TestGroup; 5] = [
    TestGroup {
        label: "library",
        target: "--lib",
    },
    TestGroup {
        label: "binary",
        target: "--bin=block-storage-ublk",
    },
    TestGroup {
        label: "lab",
        target: "--bin=block-storage-lab",
    },
    TestGroup {
        label: "mount-helper",
        target: "--bin=block-storage-mount",
    },
    TestGroup {
        label: "doc",
        target: "--doc",
    },
];

pub fn run(config: Config, repo: &Path, mut evidence: Option<&mut Evidence>) -> io::Result<bool> {
    process::become_child_subreaper()?;
    let rust = repo.join("rust");
    let suite_deadline = Instant::now() + config.suite;
    if let Some(log) = evidence.as_deref_mut() {
        log.line(&format!("working directory: {}", rust.display()))?;
        if config.mode == Mode::EngineCrash {
            log.line("environment: BLOCK_STORAGE_CRASH_TEST_MODE=acceptance")?;
        }
        log.line(&format!(
            "deadlines: per-test={}s suite={}s slow={}s",
            config.per_test.as_secs(),
            config.suite.as_secs(),
            config.slow.as_secs()
        ))?;
    }

    for group in GROUPS {
        let list = cargo_command(
            config.mode,
            &rust,
            &["test", "--quiet", group.target, "--", "--list"],
        );
        let list_result = run_captured(list, suite_deadline, evidence.as_deref_mut())?;
        if !matches!(list_result.outcome, Outcome::Exit(ref status) if status.success()) {
            list_result.print_to_stderr()?;
            eprintln!("FAILED: could not list [{}] tests", group.label);
            return Ok(false);
        }
        let tests = parse_test_list(&list_result.text()?);
        if tests.is_empty() {
            println!("[{}] no tests", group.label);
            continue;
        }

        for test_name in tests {
            if Instant::now() >= suite_deadline {
                eprintln!(
                    "FAILED: test suite exceeded {}s deadline",
                    config.suite.as_secs()
                );
                return Ok(false);
            }
            let command = cargo_command(
                config.mode,
                &rust,
                &["test", "--quiet", group.target, &test_name, "--", "--exact"],
            );
            let started = Instant::now();
            let deadline = suite_deadline.min(started + config.per_test);
            let result = run_captured(command, deadline, evidence.as_deref_mut())?;
            let elapsed = started.elapsed();
            match &result.outcome {
                Outcome::Exit(status) if status.success() => println!(
                    "PASS [{}] {} {}{}",
                    group.label,
                    test_name,
                    format_duration(elapsed),
                    slow_marker(elapsed, config.slow)
                ),
                Outcome::Timeout => {
                    result.print_to_stderr()?;
                    if Instant::now() >= suite_deadline {
                        eprintln!(
                            "FAILED: test suite exceeded {}s deadline",
                            config.suite.as_secs()
                        );
                    } else {
                        eprintln!(
                            "TIMEOUT [{}] {} {} (deadline {}s)",
                            group.label,
                            test_name,
                            format_duration(elapsed),
                            config.per_test.as_secs()
                        );
                    }
                    return Ok(false);
                }
                Outcome::Exit(status) => {
                    result.print_to_stderr()?;
                    eprintln!(
                        "FAIL [{}] {} {} ({})",
                        group.label,
                        test_name,
                        format_duration(elapsed),
                        status
                    );
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn cargo_command(mode: Mode, directory: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("cargo");
    command.args(args).current_dir(directory);
    if mode == Mode::EngineCrash {
        command.env(CRASH_MODE_ENV, "acceptance");
    }
    command
}

struct Captured {
    outcome: Outcome,
    output: TemporaryOutput,
}

impl Captured {
    fn text(&self) -> io::Result<String> {
        self.output.text()
    }

    fn print_to_stderr(&self) -> io::Result<()> {
        eprint!("{}", self.text()?);
        Ok(())
    }
}

fn run_captured(
    mut command: Command,
    deadline: Instant,
    mut evidence: Option<&mut Evidence>,
) -> io::Result<Captured> {
    let mut output = TemporaryOutput::create()?;
    let command_text = format_command(&command);
    if let Some(log) = evidence.as_deref_mut() {
        log.line(&format!("command: {command_text}"))?;
    }
    let started = Instant::now();
    let result = process::run(&mut command, &output.file, deadline);
    let elapsed = started.elapsed();
    match result {
        Ok(outcome) => {
            if let Some(log) = evidence {
                log.line(&format!(
                    "timing: {} result: {}",
                    format_duration(elapsed),
                    outcome_text(&outcome)
                ))?;
            }
            output.rewind()?;
            Ok(Captured { outcome, output })
        }
        Err(error) => {
            if let Some(log) = evidence {
                log.line(&format!(
                    "timing: {} result: error: {error}",
                    format_duration(elapsed)
                ))?;
            }
            Err(error)
        }
    }
}

fn format_command(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Exit(status) => format!("{status}"),
        Outcome::Timeout => "timeout".to_owned(),
    }
}

pub fn parse_test_list(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

pub fn format_duration(duration: Duration) -> String {
    format!("{}.{:03}s", duration.as_secs(), duration.subsec_millis())
}

pub fn slow_marker(elapsed: Duration, threshold: Duration) -> String {
    if elapsed > threshold {
        format!(" SLOW(>{}s)", threshold.as_secs())
    } else {
        String::new()
    }
}

struct TemporaryOutput {
    file: File,
    path: PathBuf,
}

impl TemporaryOutput {
    fn create() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "block-storage-lab-output-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self { file, path })
    }

    fn rewind(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0)).map(|_| ())
    }

    fn text(&self) -> io::Result<String> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            eprintln!("failed to remove {}: {error}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_test_entries() {
        assert_eq!(
            parse_test_list("alpha: test\nnoise\nbeta::case: test\n\n2 tests, 0 benchmarks\n"),
            ["alpha", "beta::case"]
        );
    }

    #[test]
    fn classifies_and_formats_timings() {
        assert_eq!(format_duration(Duration::from_millis(1234)), "1.234s");
        assert_eq!(
            slow_marker(Duration::from_secs(1), Duration::from_secs(1)),
            ""
        );
        assert_eq!(
            slow_marker(Duration::from_millis(1001), Duration::from_secs(1)),
            " SLOW(>1s)"
        );
    }
}
