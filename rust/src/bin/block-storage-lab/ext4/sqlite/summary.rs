use super::*;
use std::collections::BTreeSet;

pub(super) struct SuiteIndex {
    filesystem: Filesystem,
    planned: Vec<matrix::Selection>,
    attempted: usize,
    completed: Vec<String>,
    failed: Vec<String>,
    timed_out: bool,
}

impl SuiteIndex {
    pub(super) fn create(
        directory: &Path,
        filesystem: Filesystem,
        planned: &[matrix::Selection],
    ) -> io::Result<Self> {
        validate_expected_set(planned)?;
        let index = Self {
            filesystem,
            planned: planned.to_vec(),
            attempted: 0,
            completed: Vec::with_capacity(planned.len()),
            failed: Vec::new(),
            timed_out: false,
        };
        index.write(directory)?;
        Ok(index)
    }

    pub(super) fn attempted(&mut self, directory: &Path) -> io::Result<()> {
        self.attempted = self
            .attempted
            .checked_add(1)
            .ok_or_else(|| io::Error::other("SQLite attempted count overflow"))?;
        self.write(directory)
    }

    pub(super) fn completed(
        &mut self,
        directory: &Path,
        selection: matrix::Selection,
    ) -> io::Result<()> {
        self.completed.push(identity(selection));
        self.write(directory)
    }

    pub(super) fn failed(
        &mut self,
        directory: &Path,
        selection: matrix::Selection,
    ) -> io::Result<()> {
        self.failed.push(identity(selection));
        self.write(directory)
    }

    pub(super) fn timed_out(&mut self, directory: &Path) -> io::Result<()> {
        self.timed_out = true;
        self.write(directory)
    }

    fn write(&self, directory: &Path) -> io::Result<()> {
        let planned: Vec<_> = self.planned.iter().copied().map(identity).collect();
        let value = json!({
            "schema": "block-storage-sqlite-suite-index",
            "schema_version": 1,
            "filesystem": format!("{:?}", self.filesystem),
            "planned_full": matrix::MATRIX_RUNS,
            "selected": self.planned.len(),
            "actual": {
                "attempted": self.attempted,
                "completed": self.completed.len(),
                "failed": self.failed.len(),
            },
            "planned_selections": planned,
            "completed_selections": self.completed,
            "failed_selections": self.failed,
            "fail_fast": true,
            "status": if self.timed_out { "timeout" }
                else if !self.failed.is_empty() { "failed" }
                else if self.completed.len() == self.planned.len() { "pass" }
                else { "running" },
        });
        write_json_atomic(&directory.join("manifest.json"), &value)
    }
}

pub(super) fn selection_directory(root: &Path, selection: matrix::Selection) -> PathBuf {
    root.join(identity(selection))
}

pub(super) fn write(
    directory: &Path,
    filesystem: Filesystem,
    expected: &[matrix::Selection],
    elapsed: Duration,
) -> io::Result<()> {
    validate_expected_set(expected)?;
    let records = read_expected_records(directory, filesystem, expected)?;
    let aggregates = aggregate(&records)?;
    let identities: Vec<_> = expected.iter().copied().map(identity).collect();
    let value = json!({
        "schema": "block-storage-sqlite-summary",
        "schema_version": 2,
        "scope": matrix_scope(filesystem),
        "status": "pass",
        "fail_fast": true,
        "selected": expected.len(),
        "planned_full": matrix::MATRIX_RUNS,
        "elapsed_ns": duration_ns(elapsed),
        "expected_selections": identities,
        "samples": aggregates,
        "baseline": {"selected": false, "percentage_changes": {"available": false}},
        "manifest": "manifest.json",
    });
    write_new_json(&directory.join("summary.json"), &value)
}

pub(super) fn identity(selection: matrix::Selection) -> String {
    format!(
        "{}--{}--repetition-{}",
        selection.case.name(),
        selection.profile.name(),
        selection.repetition
    )
}

fn validate_expected_set(expected: &[matrix::Selection]) -> io::Result<()> {
    let unique: BTreeSet<_> = expected.iter().copied().collect();
    if unique.len() != expected.len() || expected.is_empty() {
        return Err(io::Error::other("SQLite expected selection set is invalid"));
    }
    let full = matrix::full();
    if expected.len() != 1 && expected != full {
        return Err(io::Error::other(
            "SQLite selections are neither one exact member nor the exact full matrix",
        ));
    }
    if expected.iter().any(|selection| {
        matrix::selected(selection.case, selection.profile, selection.repetition).is_err()
    }) {
        return Err(io::Error::other(
            "SQLite expected selection is not whitelisted",
        ));
    }
    Ok(())
}

fn read_expected_records(
    directory: &Path,
    filesystem: Filesystem,
    expected: &[matrix::Selection],
) -> io::Result<Vec<Value>> {
    let mut records = Vec::with_capacity(expected.len());
    for selection in expected {
        let path = selection_directory(directory, *selection).join("result.jsonl");
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines();
        let line = lines
            .next()
            .ok_or_else(|| io::Error::other("SQLite selection result is empty"))?;
        if lines.next().is_some() {
            return Err(io::Error::other("SQLite selection has multiple results"));
        }
        let record: Value = serde_json::from_str(line).map_err(io::Error::other)?;
        validate_record(&record, filesystem, *selection)?;
        records.push(record);
    }
    Ok(records)
}

fn validate_record(
    record: &Value,
    filesystem: Filesystem,
    selection: matrix::Selection,
) -> io::Result<()> {
    let labels = &record["labels"];
    let valid = record["schema"] == "block-storage-sqlite"
        && record["schema_version"] == 2
        && record["scope"] == matrix_scope(filesystem)
        && record["status"] == "pass"
        && labels["fs"] == format!("{filesystem:?}")
        && labels["case"] == selection.case.name()
        && labels["profile"] == selection.profile.name()
        && labels["repetition"] == selection.repetition
        && labels["expected"] == matrix::REPETITIONS;
    if !valid {
        return Err(io::Error::other(
            "SQLite result does not match expected selection",
        ));
    }
    Ok(())
}

fn aggregate(records: &[Value]) -> io::Result<Vec<Value>> {
    let mut groups: BTreeMap<(String, String), Vec<&Value>> = BTreeMap::new();
    for record in records {
        let labels = &record["labels"];
        let case = required_string(labels, "case")?;
        let profile = required_string(labels, "profile")?;
        groups.entry((case, profile)).or_default().push(record);
    }
    groups
        .into_iter()
        .map(|((case, profile), samples)| aggregate_group(&case, &profile, &samples))
        .collect()
}

fn aggregate_group(case: &str, profile: &str, samples: &[&Value]) -> io::Result<Value> {
    let workload = sample_u64(
        samples,
        &["phases", "workload_plus_drain", "timings", "workload_ns"],
    )?;
    let with_drain = sample_u64(
        samples,
        &[
            "phases",
            "workload_plus_drain",
            "timings",
            "workload_plus_drain_ns",
        ],
    )?;
    let throughput = sample_optional_f64(
        samples,
        &["phases", "workload_plus_drain", "operations_per_second"],
    )?;
    Ok(json!({
        "case": case,
        "profile": profile,
        "sample_count": samples.len(),
        "expected_repetitions_complete": samples.len() == usize::from(matrix::REPETITIONS),
        "workload_ns": u64_statistics(workload),
        "workload_plus_drain_ns": u64_statistics(with_drain),
        "operations_per_second": throughput.map_or_else(
            || json!({"available": false}),
            f64_statistics,
        ),
        "tail_latency": {"available": false},
    }))
}

fn sample_u64(samples: &[&Value], path: &[&str]) -> io::Result<Vec<u64>> {
    samples
        .iter()
        .map(|sample| value_at(sample, path).as_u64().ok_or_else(invalid_summary))
        .collect()
}

fn sample_optional_f64(samples: &[&Value], path: &[&str]) -> io::Result<Option<Vec<f64>>> {
    let values: Option<Vec<_>> = samples
        .iter()
        .map(|sample| value_at(sample, path).as_f64())
        .collect();
    if values
        .as_ref()
        .is_some_and(|items| items.iter().any(|item| !item.is_finite()))
    {
        return Err(invalid_summary());
    }
    Ok(values)
}

fn u64_statistics(mut samples: Vec<u64>) -> Value {
    assert!(!samples.is_empty(), "aggregate groups are nonempty");
    samples.sort_unstable();
    statistics_json(&samples, |value| json!(value))
}

fn f64_statistics(mut samples: Vec<f64>) -> Value {
    assert!(!samples.is_empty(), "aggregate groups are nonempty");
    samples.sort_by(f64::total_cmp);
    statistics_json(&samples, |value| json!(value))
}

fn statistics_json<T, F>(samples: &[T], encode: F) -> Value
where
    T: Copy,
    F: Fn(T) -> Value,
{
    let median = *samples.get(samples.len() / 2).expect("nonempty median");
    let minimum = *samples.first().expect("nonempty minimum");
    let maximum = *samples.last().expect("nonempty maximum");
    json!({
        "samples": samples.iter().copied().map(&encode).collect::<Vec<_>>(),
        "median": encode(median),
        "range": {"minimum": encode(minimum), "maximum": encode(maximum)},
    })
}

fn required_string(value: &Value, field: &str) -> io::Result<String> {
    value[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(invalid_summary)
}

fn value_at<'a>(mut value: &'a Value, path: &[&str]) -> &'a Value {
    for field in path {
        value = &value[*field];
    }
    value
}

fn write_json_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().ok_or_else(invalid_summary)?)?.sync_all()
}

fn write_new_json(path: &Path, value: &Value) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn invalid_summary() -> io::Error {
    io::Error::other("SQLite summary input is incomplete or invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_rejects_results_from_another_filesystem() {
        let selection = matrix::full()[0];
        let mut record = json!({
            "schema": "block-storage-sqlite", "schema_version": 2,
            "scope": "xfs_matrix", "status": "pass",
            "labels": {"fs": "Xfs", "case": selection.case.name(),
                       "profile": selection.profile.name(), "repetition": selection.repetition,
                       "expected": matrix::REPETITIONS},
        });
        assert!(validate_record(&record, Filesystem::Xfs, selection).is_ok());
        assert!(validate_record(&record, Filesystem::Ext4, selection).is_err());
        record["labels"]["fs"] = json!("Ext4");
        assert!(validate_record(&record, Filesystem::Xfs, selection).is_err());
    }

    #[test]
    fn timeout_preserves_partial_suite_counts() {
        let mut owned = OwnedTempDir::create(Path::new("/tmp")).unwrap();
        let mut index =
            SuiteIndex::create(owned.path(), Filesystem::Ext4, &matrix::full()).unwrap();
        index.timed_out(owned.path()).unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(owned.path().join("manifest.json")).unwrap()).unwrap();
        assert_eq!(value["status"], "timeout");
        assert_eq!(value["actual"]["attempted"], 0);
        assert_eq!(value["selected"], 87);
        owned.cleanup().unwrap();
    }

    #[test]
    fn summary_preserves_reported_throughput_exactly() {
        let input = "119551.02984339283";
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(value.as_f64(), Some(input.parse::<f64>().unwrap()));
        assert_eq!(serde_json::to_string(&value).unwrap(), input);
    }

    #[test]
    fn expected_set_is_exact() {
        assert!(validate_expected_set(&matrix::full()).is_ok());
        assert!(validate_expected_set(&matrix::full()[..2]).is_err());
        assert!(validate_expected_set(&[matrix::full()[0]]).is_ok());
    }

    #[test]
    fn manifest_tracks_attempts_and_terminal_identities() {
        let root = std::env::temp_dir().join(format!("sqlite-index-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let selection = matrix::full()[0];
        let mut index = SuiteIndex::create(&root, Filesystem::Ext4, &[selection]).unwrap();
        index.attempted(&root).unwrap();
        index.failed(&root, selection).unwrap();
        let manifest: Value =
            serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["planned_full"], 87);
        assert_eq!(manifest["selected"], 1);
        assert_eq!(manifest["actual"]["attempted"], 1);
        assert_eq!(manifest["actual"]["completed"], 0);
        assert_eq!(manifest["actual"]["failed"], 1);
        assert_eq!(manifest["failed_selections"][0], identity(selection));
        assert!(!root.join("manifest.json.tmp").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identity_is_stable() {
        assert_eq!(
            identity(matrix::selected(Case::SpeedtestMain, Profile::WalFull, 2).unwrap()),
            "speedtest-main--wal-full--repetition-2"
        );
    }
}
