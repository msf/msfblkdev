use super::params::SPEEDTEST_MAIN_IDS;
use super::*;

pub(super) struct ToolOutput {
    pub(super) metrics: Value,
    pub(super) verification: Option<String>,
}

pub(super) fn parse_workload_stdout(
    bytes: &[u8],
    case: Case,
    profile: Profile,
) -> io::Result<ToolOutput> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    if text.contains("ERROR:") {
        return Err(io::Error::other("SQLite workload reported an error"));
    }
    if case.is_speedtest() {
        return parse_speedtest_output(text);
    }
    parse_kv_output(text, case, profile)
}

fn parse_kv_output(text: &str, case: Case, profile: Profile) -> io::Result<ToolOutput> {
    let order = if case.random() { "--random" } else { "--asc" };
    let requested = format!(
        "--count {} --max-id {} {order}",
        case.operations(),
        case.dataset_blobs()
    );
    let jmode = format!("--cache-size 256 --jmode {}", profile.journal());
    for required in [
        requested.as_str(),
        jmode.as_str(),
        "--mmap 0",
        "Database page size: 4096",
    ] {
        if !text.lines().any(|line| line == required) {
            return Err(io::Error::other(
                "kvtest did not confirm requested parameters",
            ));
        }
    }
    if text.contains("--blob-api") {
        return Err(io::Error::other("kvtest unexpectedly used the BLOB API"));
    }
    let operation = if case.updates() { "write" } else { "read" };
    let elapsed_raw = prefixed_stat(text, "Total elapsed time: ")?;
    let micros_raw = prefixed_stat(text, &format!("Microseconds per BLOB {operation}: "))?;
    let rate_raw = suffixed_stat(text, &format!("Content {operation} rate: "), " MB/s")?;
    let (elapsed, elapsed_value) = upstream_metric(elapsed_raw)?;
    let (micros, _) = upstream_metric(micros_raw)?;
    let (rate, _) = upstream_metric(rate_raw)?;
    let derived_rate = elapsed_value.map_or_else(
        || json!({"available": false, "reason": "upstream elapsed timing is zero or non-finite"}),
        |seconds| json!({"available": true, "value": case.operations() as f64 / seconds}),
    );
    Ok(ToolOutput {
        metrics: json!({
            "elapsed_seconds_wall_clock": elapsed,
            "operation": operation,
            "microseconds_per_operation": micros,
            "content_rate_mb_s": rate,
            "derived_operations_per_second": derived_rate,
            "statistics_raw_in_commands_log": true,
        }),
        verification: None,
    })
}

fn parse_speedtest_output(text: &str) -> io::Result<ToolOutput> {
    let verification = text
        .lines()
        .find_map(|line| line.strip_prefix("Verification Hash: "))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::other("speedtest1 verification hash is absent"))?;
    let tests: Vec<_> = text
        .lines()
        .filter_map(parse_speedtest_result)
        .collect::<io::Result<_>>()?;
    let actual_ids: Option<Vec<_>> = tests.iter().map(|result| result["id"].as_u64()).collect();
    if actual_ids.as_deref() != Some(SPEEDTEST_MAIN_IDS.as_slice())
        || !text.contains("       TOTAL")
    {
        return Err(io::Error::other(
            "speedtest1 main output has an incomplete numbered subtest set",
        ));
    }
    Ok(ToolOutput {
        metrics: json!({
            "size": 1,
            "testset": "main",
            "built_in_verification": true,
            "completed_subtests": {"unit": "subtest", "count": tests.len(),
                                   "expected": SPEEDTEST_MAIN_IDS.len()},
            "individual_results": tests,
            "VerificationHash": verification,
            "statistics_raw_in_commands_log": true,
        }),
        verification: Some(verification.to_owned()),
    })
}

fn parse_speedtest_result(line: &str) -> Option<io::Result<Value>> {
    let trimmed = line.trim_start();
    if !trimmed.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some((|| {
        let (body, seconds) = trimmed
            .rsplit_once(char::is_whitespace)
            .ok_or_else(|| io::Error::other("invalid speedtest1 result row"))?;
        let seconds = seconds
            .strip_suffix('s')
            .ok_or_else(|| io::Error::other("invalid speedtest1 timing suffix"))?;
        let (elapsed, _) = upstream_metric(seconds)?;
        let (identifier, description) = body
            .split_once(char::is_whitespace)
            .ok_or_else(|| io::Error::other("invalid speedtest1 result identity"))?;
        let identifier = identifier.parse::<u64>().map_err(io::Error::other)?;
        Ok(json!({
            "id": identifier,
            "description_and_leader": description.trim(),
            "elapsed_seconds": elapsed,
            "raw": line,
        }))
    })())
}

fn prefixed_stat<'a>(text: &'a str, prefix: &str) -> io::Result<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .ok_or_else(|| io::Error::other("required upstream statistic is absent"))
}

fn suffixed_stat<'a>(text: &'a str, prefix: &str, suffix: &str) -> io::Result<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|value| value.strip_suffix(suffix))
        .ok_or_else(|| io::Error::other("required upstream statistic is absent"))
}

fn upstream_metric(raw: &str) -> io::Result<(Value, Option<f64>)> {
    let value = raw.parse::<f64>().map_err(io::Error::other)?;
    if value.is_nan() || value < 0.0 {
        return Err(io::Error::other("upstream statistic is invalid"));
    }
    if value.is_finite() && value > 0.0 {
        return Ok((
            json!({"available": true, "value": value, "raw": raw}),
            Some(value),
        ));
    }
    Ok((
        json!({"available": false, "reason": "upstream value is zero or non-finite", "raw": raw}),
        None,
    ))
}

pub(super) fn parse_validation(bytes: &[u8]) -> io::Result<String> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != 2 || lines[0] != "ok" {
        return Err(io::Error::other(
            "read-only SQLite validation output is not exact",
        ));
    }
    let digest = lines[1];
    if digest.len() != 56
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::other(
            "SQLite .sha3sum --schema digest is invalid",
        ));
    }
    Ok(digest.to_owned())
}

pub(super) fn filter_evidence_directory(
    text: &str,
    repo: &Path,
    directory: &Path,
) -> io::Result<String> {
    let relative = directory
        .strip_prefix(repo)
        .map_err(|_| io::Error::other("SQLite evidence is outside repository"))?;
    Ok(text
        .lines()
        .filter(|line| {
            let Some(untracked) = line.strip_prefix("?? ") else {
                return true;
            };
            !relative.starts_with(untracked.trim_end_matches('/'))
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(super) fn parse_sha256sum(text: &str, expected_path: &Path) -> io::Result<String> {
    let (digest, path) = text
        .trim_end()
        .split_once("  ")
        .ok_or_else(|| io::Error::other("invalid sha256sum output"))?;
    if Path::new(path) != expected_path
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::other("invalid SHA256 digest or path"));
    }
    Ok(digest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speedtest_parser_requires_all_main_subtests_and_marks_zero_unavailable() {
        let rows = SPEEDTEST_MAIN_IDS
            .iter()
            .map(|id| format!(" {id} - fixture row................    0.000s"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = format!(
            "{rows}\n       TOTAL................    0.000s\nVerification Hash: fixture-hash\n"
        );
        let parsed = parse_speedtest_output(&output).unwrap();
        assert_eq!(parsed.metrics["completed_subtests"]["count"], 32);
        assert_eq!(parsed.metrics["completed_subtests"]["unit"], "subtest");
        assert_eq!(parsed.metrics["VerificationHash"], "fixture-hash");
        assert_eq!(
            parsed.metrics["individual_results"][0]["elapsed_seconds"]["available"],
            false
        );
        assert_eq!(
            parsed.metrics["individual_results"][0]["elapsed_seconds"]["raw"],
            "0.000"
        );
        assert_eq!(
            parsed.metrics["individual_results"][0]["raw"],
            " 100 - fixture row................    0.000s"
        );
        assert!(parse_speedtest_output(&output.replacen(" 100 -", " x -", 1)).is_err());

        let positive = parse_speedtest_result(" 100 - row    0.125s")
            .unwrap()
            .unwrap();
        assert_eq!(positive["elapsed_seconds"]["available"], true);
        assert_eq!(positive["elapsed_seconds"]["value"], 0.125);
    }

    #[test]
    fn kv_parser_retains_zero_and_infinite_coarse_timings_as_unavailable() {
        let text = "--count 256 --max-id 32 --random\n\
                        --cache-size 256 --jmode wal\n\
                        --mmap 0\n\
                        Database page size: 4096\n\
                        Total elapsed time: 0\n\
                        Microseconds per BLOB write: inf\n\
                        Content write rate: inf MB/s\n";
        let parsed =
            parse_kv_output(text, Case::KvRandomUpdateLargePerOpCommit, Profile::WalFull).unwrap();
        assert_eq!(
            parsed.metrics["elapsed_seconds_wall_clock"]["available"],
            false
        );
        assert_eq!(parsed.metrics["elapsed_seconds_wall_clock"]["raw"], "0");
        assert_eq!(
            parsed.metrics["microseconds_per_operation"]["available"],
            false
        );
        assert_eq!(parsed.metrics["content_rate_mb_s"]["raw"], "inf");
        assert_eq!(
            parsed.metrics["derived_operations_per_second"]["available"],
            false
        );
    }
}
