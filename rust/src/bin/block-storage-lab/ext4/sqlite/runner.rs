use super::*;

pub(crate) fn run(repo: &Path, per_command: Duration, suite: Duration) -> io::Result<()> {
    let started = Instant::now();
    let (mut evidence, directory) = Evidence::create_sqlite(repo)?;
    println!("evidence: {}", directory.display());
    evidence
        .line("sqlite probe: one ext4 WAL/FULL seeded-random kvtest case; not ADR acceptance")?;
    let mut lab = match new_sqlite_lab(&mut evidence, per_command, started + suite, started) {
        Ok(lab) => lab,
        Err(error) => {
            write_fallback_result(&directory, started.elapsed(), &error, None)?;
            return Err(error);
        }
    };
    execute_case(&mut lab, repo, "bounded probe only")
}

pub(crate) fn run_ext4(
    repo: &Path,
    per_command: Duration,
    suite: Duration,
    selector: Option<crate::config::SqliteSelector>,
) -> io::Result<()> {
    let started = Instant::now();
    let deadline = started + suite;
    let selections = match selector {
        Some(value) => vec![matrix::selected(
            value.case,
            value.profile,
            value.repetition,
        )?],
        None => matrix::full(),
    };
    let (mut evidence, directory) = Evidence::create_sqlite(repo)?;
    println!("evidence: {}", directory.display());
    evidence.line(&format!(
        "sqlite ext4 matrix: selected={} planned_full={} fail_fast=true",
        selections.len(),
        matrix::MATRIX_RUNS
    ))?;
    let mut index = summary::SuiteIndex::create(&directory, &selections)?;
    for (ordinal, selection) in selections.iter().copied().enumerate() {
        if Instant::now() >= deadline {
            index.timed_out(&directory)?;
            evidence.line("result: TIMEOUT; suite deadline reached before next selection")?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SQLite ext4 suite deadline reached before next selection",
            ));
        }
        evidence.line(&format!(
            "selection {}/{}: {}",
            ordinal + 1,
            selections.len(),
            summary::identity(selection)
        ))?;
        index.attempted(&directory)?;
        if let Err(error) =
            execute_selection(repo, &directory, selection, per_command, deadline, started)
        {
            index.failed(&directory, selection)?;
            return Err(error);
        }
        index.completed(&directory, selection)?;
    }
    summary::write(&directory, &selections, started.elapsed())?;
    evidence.line("result: PASS; every exact selected SQLite ext4 case passed")
}

fn execute_selection(
    repo: &Path,
    root: &Path,
    selection: matrix::Selection,
    per_command: Duration,
    deadline: Instant,
    started: Instant,
) -> io::Result<()> {
    let case_started = Instant::now();
    let directory = summary::selection_directory(root, selection);
    let mut evidence = Evidence::create_in_directory(&directory)?;
    evidence.line(&format!("selection: {}", summary::identity(selection)))?;
    let mut lab = match new_sqlite_lab(&mut evidence, per_command, deadline, started) {
        Ok(lab) => lab,
        Err(error) => {
            write_fallback_result(&directory, case_started.elapsed(), &error, Some(selection))?;
            return Err(error);
        }
    };
    lab.sqlite_state_mut()?.select(selection);
    execute_case(&mut lab, repo, "SQLite ext4 matrix selection")
}

fn new_sqlite_lab<'a>(
    evidence: &'a mut Evidence,
    per_command: Duration,
    deadline: Instant,
    started: Instant,
) -> io::Result<Lab<'a>> {
    Lab::new(
        Filesystem::Ext4,
        WorkloadMode::Sqlite,
        evidence,
        per_command,
        deadline,
        started,
    )
}

fn execute_case(lab: &mut Lab<'_>, repo: &Path, success: &str) -> io::Result<()> {
    let execution = lab.preflight(repo).and_then(|()| lab.lifecycle());
    if let Err(error) = execution {
        record_failed_case(lab, &error)?;
        return Err(error);
    }
    let cleanup_started = Instant::now();
    if let Err(error) = lab.finish() {
        lab.sqlite_write_result(status_for(&error), Some(&error))?;
        return Err(error);
    }
    lab.sqlite_state_mut()?.phases.insert(
        "cleanup".into(),
        json!({"duration_ns": duration_ns(cleanup_started.elapsed()),
               "io": {"available": false, "reason": "device was cleanly stopped before exact fixture cleanup"}}),
    );
    lab.sqlite_write_result("pass", None)?;
    lab.evidence.line(&format!("result: PASS; {success}"))
}

fn record_failed_case(lab: &mut Lab<'_>, error: &io::Error) -> io::Result<()> {
    let cleanup_started = Instant::now();
    let cleanup = lab.recover_error();
    lab.sqlite_state_mut()?.phases.insert(
        "failure_cleanup".into(),
        json!({"duration_ns": duration_ns(cleanup_started.elapsed()), "completed": cleanup.is_ok()}),
    );
    if let Err(cleanup_error) = cleanup {
        lab.sqlite_state_mut()?
            .errors
            .push(format!("cleanup: {cleanup_error}"));
    }
    if lab.sqlite_state()?.reader.is_some()
        && lab.daemon.is_none()
        && let Err(ledger_error) = lab.sqlite_finalize_ledger()
    {
        lab.sqlite_state_mut()?
            .errors
            .push(format!("ledger: {ledger_error}"));
        lab.sqlite_state_mut()?.reader = None;
    }
    lab.sqlite_write_result(status_for(error), Some(error))
}

fn write_fallback_result(
    directory: &Path,
    elapsed: Duration,
    error: &io::Error,
    selection: Option<matrix::Selection>,
) -> io::Result<()> {
    let (scope, case, profile, repetition) = selection.map_or(
        ("probe", Case::ProbeSeededRandomUpdate, Profile::WalFull, 1),
        |value| ("ext4_matrix", value.case, value.profile, value.repetition),
    );
    let path = directory.join("result.jsonl");
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let value = json!({
        "schema": "block-storage-sqlite", "schema_version": 2, "scope": scope,
        "status": status_for(error),
        "labels": {"fs": "Ext4", "case": case.name(), "profile": profile.name(),
                   "repetition": repetition,
                   "expected": if selection.is_some() { matrix::REPETITIONS } else { 1 }},
        "full_case_elapsed_ns": duration_ns(elapsed), "errors": [error.to_string()],
        "partial": true,
    });
    serde_json::to_writer(&mut file, &value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_and_timeout_json_preserve_status() {
        let failed = io::Error::other("boom");
        let timeout = io::Error::new(io::ErrorKind::TimedOut, "late");
        for (label, error, expected) in [
            ("failed", failed, "failed"),
            ("timeout", timeout, "timeout"),
        ] {
            let directory =
                std::env::temp_dir().join(format!("sqlite-result-{label}-{}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            write_fallback_result(&directory, Duration::from_nanos(7), &error, None).unwrap();
            let line = fs::read_to_string(directory.join("result.jsonl")).unwrap();
            let value: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(value["status"], expected);
            assert_eq!(value["errors"][0], error.to_string());
            fs::remove_dir_all(directory).unwrap();
        }
    }
}
