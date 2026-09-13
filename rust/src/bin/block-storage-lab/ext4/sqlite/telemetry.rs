use super::*;

const WORKLOAD_CONNECTION: u64 = 2;

pub(super) struct Parsed {
    pub(super) value: Value,
    pub(super) process_duration_ns: u64,
    pub(super) snapshot_overhead_ns: u64,
    pub(super) drain_close_ns: u64,
    pub(super) drain_start: IoPoint,
    pub(super) drain_end: IoPoint,
}

#[derive(Default)]
struct Records {
    settings_open: u64,
    settings_close: u64,
    generators: u64,
    automatic: u64,
    final_checkpoints: u64,
    drain: Option<(u64, u64)>,
    process: Option<(u64, u64)>,
    drain_start: Option<SnapshotPoint>,
    drain_end: Option<SnapshotPoint>,
}

struct SnapshotPoint {
    monotonic_ns: u64,
    io: IoPoint,
    ledger_json: Value,
}

struct Summary {
    process_duration_ns: u64,
    snapshot_overhead_ns: u64,
    drain_close_ns: u64,
    observer_overhead_ns: u64,
    automatic_checkpoints: u64,
}

pub(super) fn parse(
    bytes: &[u8],
    profile: Profile,
    snapshots_required: bool,
) -> io::Result<Parsed> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let mut records = Records::default();
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("sqlite-lab ") else {
            continue;
        };
        let record: Value = serde_json::from_str(payload).map_err(io::Error::other)?;
        parse_record(&record, profile, &mut records)?;
    }
    finish(records, profile, snapshots_required)
}

fn parse_record(record: &Value, profile: Profile, records: &mut Records) -> io::Result<()> {
    let kind = required_str(record, "record")?;
    if kind.contains("error") {
        return Err(invalid_telemetry());
    }
    let connection = record["connection"].as_u64();
    match kind {
        "generator" if connection == Some(WORKLOAD_CONNECTION) => {
            if record["seed"] != SEED || record["identity"] != "splitmix64-byte-v1" {
                return Err(invalid_telemetry());
            }
            records.generators += 1;
        }
        "settings" if connection == Some(WORKLOAD_CONNECTION) => {
            validate_settings(record, profile)?;
            match record["phase"].as_str() {
                Some("open") => records.settings_open += 1,
                Some("close") => records.settings_close += 1,
                _ => return Err(invalid_telemetry()),
            }
        }
        "auto_checkpoint" => parse_auto_checkpoint(record, records)?,
        "checkpoint" if connection == Some(WORKLOAD_CONNECTION) => {
            if record["mode"] != "truncate" || record["rc"] != 0 || record["status"] != "completed"
            {
                return Err(invalid_telemetry());
            }
            records.final_checkpoints += 1;
        }
        "device_snapshot" if connection == Some(WORKLOAD_CONNECTION) => {
            let point = parse_snapshot(record)?;
            let slot = match record["phase"].as_str() {
                Some("drain_start") => &mut records.drain_start,
                Some("drain_end") => &mut records.drain_end,
                _ => return Err(invalid_telemetry()),
            };
            if slot.replace(point).is_some() {
                return Err(invalid_telemetry());
            }
        }
        "drain_close" if connection == Some(WORKLOAD_CONNECTION) => {
            if record["checkpoint_rc"] != 0
                || record["close_rc"] != 0
                || record["status"] != "completed"
                || records.drain.is_some()
            {
                return Err(invalid_telemetry());
            }
            records.drain = Some((
                required_u64(record, "duration_ns")?,
                required_u64(record, "observer_overhead_ns")?,
            ));
        }
        "process" => {
            if record["status"] != "completed" || records.process.is_some() {
                return Err(invalid_telemetry());
            }
            records.process = Some((
                required_u64(record, "duration_ns")?,
                required_u64(record, "total_snapshot_overhead_ns")?,
            ));
        }
        _ => {}
    }
    Ok(())
}

fn parse_auto_checkpoint(record: &Value, records: &mut Records) -> io::Result<()> {
    if record["database"] != "main"
        || record["policy"] != "passive"
        || record["rc"] != 0
        || record["trigger_frames"]
            .as_u64()
            .is_none_or(|value| value < 1000)
        || record["checkpointed_frames"]
            .as_u64()
            .is_none_or(|value| value < 1000)
    {
        return Err(invalid_telemetry());
    }
    records.automatic += 1;
    Ok(())
}

fn parse_snapshot(record: &Value) -> io::Result<SnapshotPoint> {
    let block = &record["block"];
    let ledger = &record["ledger"];
    let write_limit = required_u64(ledger, "write_limit_bytes")?;
    let reserved = required_u64(ledger, "write_reserved_bytes")?;
    let _pending = required_u64(ledger, "pending_operations")?;
    if write_limit != WRITE_LIMIT_BYTES
        || reserved > write_limit
        || required_u64(ledger, "errors")? != 0
        || required_bool(ledger, "completed_totals_incomplete")?
        || required_bool(ledger, "budget_exhausted")?
    {
        return Err(invalid_telemetry());
    }
    Ok(SnapshotPoint {
        monotonic_ns: required_u64(record, "monotonic_ns")?,
        io: IoPoint {
            block: BlockCounters {
                reads: required_u64(block, "reads")?,
                writes: required_u64(block, "writes")?,
                flushes: required_u64(block, "flushes")?,
            },
            ledger: LedgerCounters {
                read_bytes: required_u64(ledger, "read_completed_bytes")?,
                write_completed_bytes: required_u64(ledger, "write_completed_bytes")?,
                write_reserved_bytes: reserved,
                reads: required_u64(ledger, "read_completed_operations")?,
                writes: required_u64(ledger, "write_completed_operations")?,
                flushes: required_u64(ledger, "flush_completed_operations")?,
            },
        },
        ledger_json: ledger.clone(),
    })
}

fn finish(records: Records, profile: Profile, snapshots_required: bool) -> io::Result<Parsed> {
    if (
        records.settings_open,
        records.settings_close,
        records.generators,
        records.final_checkpoints,
    ) != (1, 1, 1, 1)
        || records.automatic < 2
    {
        return Err(invalid_telemetry());
    }
    let process = records.process.ok_or_else(invalid_telemetry)?;
    let drain = records.drain.ok_or_else(invalid_telemetry)?;
    let automatic = records.automatic;
    let (drain_start, drain_end) = snapshot_pair(records, snapshots_required)?;
    let (process_duration_ns, snapshot_overhead_ns) = process;
    let (drain_close_ns, observer_overhead_ns) = drain;
    if process_duration_ns == 0
        || drain_close_ns == 0
        || observer_overhead_ns != snapshot_overhead_ns
    {
        return Err(invalid_telemetry());
    }
    let summary = Summary {
        process_duration_ns,
        snapshot_overhead_ns,
        drain_close_ns,
        observer_overhead_ns,
        automatic_checkpoints: automatic,
    };
    let value = telemetry_json(profile, &drain_start, &drain_end, &summary);
    Ok(Parsed {
        value,
        process_duration_ns,
        snapshot_overhead_ns,
        drain_close_ns,
        drain_start: drain_start.io,
        drain_end: drain_end.io,
    })
}

fn snapshot_pair(records: Records, required: bool) -> io::Result<(SnapshotPoint, SnapshotPoint)> {
    if !required {
        if records.drain_start.is_some() || records.drain_end.is_some() {
            return Err(invalid_telemetry());
        }
        return Ok((empty_snapshot(), empty_snapshot()));
    }
    let start = records.drain_start.ok_or_else(invalid_telemetry)?;
    let end = records.drain_end.ok_or_else(invalid_telemetry)?;
    if end.monotonic_ns < start.monotonic_ns || io_delta(&start.io, &end.io).is_err() {
        return Err(invalid_telemetry());
    }
    Ok((start, end))
}

fn empty_snapshot() -> SnapshotPoint {
    SnapshotPoint {
        monotonic_ns: 0,
        io: IoPoint::default(),
        ledger_json: Value::Null,
    }
}

fn telemetry_json(
    profile: Profile,
    start: &SnapshotPoint,
    end: &SnapshotPoint,
    summary: &Summary,
) -> Value {
    let snapshots = if start.monotonic_ns == 0 {
        json!({"enabled": false})
    } else {
        json!({
            "enabled": true,
            "connection": WORKLOAD_CONNECTION,
            "drain_start": {"monotonic_ns": start.monotonic_ns, "block": block_json(&start.io.block), "ledger": start.ledger_json},
            "drain_end": {"monotonic_ns": end.monotonic_ns, "block": block_json(&end.io.block), "ledger": end.ledger_json},
            "counters_observed_around_drain": true,
            "api_duration_excludes_observer_overhead": true,
        })
    };
    json!({
        "profile": profile.name(), "workload_connection": WORKLOAD_CONNECTION,
        "preparatory_connection_drains_ignored": true,
        "automatic_passive_checkpoints_completed": summary.automatic_checkpoints,
        "automatic_checkpoint_minimum": 2,
        "final_truncate_checkpoint": "completed",
        "process": {"duration_ns": summary.process_duration_ns, "total_snapshot_overhead_ns": summary.snapshot_overhead_ns, "status": "completed"},
        "selected_drain_close": {"duration_ns": summary.drain_close_ns, "observer_overhead_ns": summary.observer_overhead_ns, "status": "completed"},
        "device_snapshots": snapshots,
        "errors": 0,
    })
}

fn block_json(block: &BlockCounters) -> Value {
    json!({"reads": block.reads, "writes": block.writes, "flushes": block.flushes})
}

fn validate_settings(record: &Value, profile: Profile) -> io::Result<()> {
    if record["profile"] != profile.name()
        || record["page_size"] != PAGE_BYTES
        || record["cache_pages"] != CACHE_PAGES
        || record["mmap_bytes"] != MMAP_BYTES
        || record["journal_mode"] != profile.journal()
        || record["synchronous"] != profile.synchronous()
        || record["wal_autocheckpoint"] != 1000
        || record["rc"] != 0
    {
        return Err(invalid_telemetry());
    }
    Ok(())
}

fn required_u64(value: &Value, field: &str) -> io::Result<u64> {
    value[field].as_u64().ok_or_else(invalid_telemetry)
}

fn required_bool(value: &Value, field: &str) -> io::Result<bool> {
    value[field].as_bool().ok_or_else(invalid_telemetry)
}

fn required_str<'a>(value: &'a Value, field: &str) -> io::Result<&'a str> {
    value[field].as_str().ok_or_else(invalid_telemetry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(phase: &str, monotonic_ns: u64, value: u64) -> String {
        format!(
            "sqlite-lab {{\"record\":\"device_snapshot\",\"phase\":\"{phase}\",\"connection\":2,\"monotonic_ns\":{monotonic_ns},\"block\":{{\"reads\":{value},\"writes\":{value},\"flushes\":{value}}},\"ledger\":{{\"write_limit_bytes\":536870912,\"write_reserved_bytes\":{value},\"write_completed_bytes\":{value},\"read_completed_bytes\":{value},\"read_completed_operations\":{value},\"write_completed_operations\":{value},\"flush_completed_operations\":{value},\"pending_operations\":3,\"errors\":0,\"completed_totals_incomplete\":false,\"budget_exhausted\":false}}}}\n"
        )
    }

    fn complete() -> String {
        format!(
            "sqlite-lab {{\"record\":\"generator\",\"connection\":2,\"identity\":\"splitmix64-byte-v1\",\"seed\":\"{SEED}\"}}\n\
             sqlite-lab {{\"record\":\"settings\",\"phase\":\"open\",\"connection\":2,\"profile\":\"wal-full\",\"page_size\":4096,\"cache_pages\":256,\"mmap_bytes\":0,\"journal_mode\":\"wal\",\"synchronous\":2,\"wal_autocheckpoint\":1000,\"rc\":0}}\n\
             sqlite-lab {{\"record\":\"auto_checkpoint\",\"database\":\"main\",\"trigger_frames\":1002,\"rc\":0,\"checkpointed_frames\":1002,\"policy\":\"passive\"}}\n\
             sqlite-lab {{\"record\":\"auto_checkpoint\",\"database\":\"main\",\"trigger_frames\":1002,\"rc\":0,\"checkpointed_frames\":1002,\"policy\":\"passive\"}}\n\
             sqlite-lab {{\"record\":\"settings\",\"phase\":\"close\",\"connection\":2,\"profile\":\"wal-full\",\"page_size\":4096,\"cache_pages\":256,\"mmap_bytes\":0,\"journal_mode\":\"wal\",\"synchronous\":2,\"wal_autocheckpoint\":1000,\"rc\":0}}\n\
             {}{}sqlite-lab {{\"record\":\"checkpoint\",\"connection\":2,\"mode\":\"truncate\",\"rc\":0,\"status\":\"completed\"}}\n\
             sqlite-lab {{\"record\":\"drain_close\",\"connection\":2,\"duration_ns\":100,\"observer_overhead_ns\":20,\"checkpoint_rc\":0,\"close_rc\":0,\"status\":\"completed\"}}\n\
             sqlite-lab {{\"record\":\"process\",\"duration_ns\":1000,\"total_snapshot_overhead_ns\":20,\"status\":\"completed\"}}\n",
            snapshot("drain_start", 10, 10),
            snapshot("drain_end", 20, 15)
        )
    }

    #[test]
    fn paired_snapshots_and_process_are_exact() {
        let parsed = parse(complete().as_bytes(), Profile::WalFull, true).unwrap();
        assert_eq!(parsed.process_duration_ns, 1000);
        assert_eq!(parsed.snapshot_overhead_ns, 20);
        assert_eq!(parsed.drain_close_ns, 100);
        assert_eq!(
            io_delta(&parsed.drain_start, &parsed.drain_end).unwrap()["ublk_completed_writes"],
            5
        );
        assert_eq!(
            parsed.value["device_snapshots"]["counters_observed_around_drain"],
            true
        );
    }

    #[test]
    fn reference_requires_process_but_forbids_snapshots() {
        let without = complete()
            .lines()
            .filter(|line| !line.contains("device_snapshot"))
            .collect::<Vec<_>>()
            .join("\n");
        let without = without
            .replace("\"observer_overhead_ns\":20", "\"observer_overhead_ns\":0")
            .replace(
                "\"total_snapshot_overhead_ns\":20",
                "\"total_snapshot_overhead_ns\":0",
            );
        assert!(parse(without.as_bytes(), Profile::WalFull, false).is_ok());
        assert!(parse(complete().as_bytes(), Profile::WalFull, false).is_err());
        let no_process = without
            .lines()
            .filter(|line| !line.contains("\"process\""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(no_process.as_bytes(), Profile::WalFull, false).is_err());
    }

    #[test]
    fn rejects_missing_duplicate_decreasing_and_invalid_snapshot_records() {
        let complete = complete();
        let missing = complete
            .lines()
            .filter(|line| !line.contains("drain_end"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(missing.as_bytes(), Profile::WalFull, true).is_err());
        let duplicate = format!("{complete}{}", snapshot("drain_start", 11, 11));
        assert!(parse(duplicate.as_bytes(), Profile::WalFull, true).is_err());
        let decreasing =
            complete.replace(&snapshot("drain_end", 20, 15), &snapshot("drain_end", 9, 9));
        assert!(parse(decreasing.as_bytes(), Profile::WalFull, true).is_err());
        let error = complete.replace("\"errors\":0", "\"errors\":1");
        assert!(parse(error.as_bytes(), Profile::WalFull, true).is_err());
        let incomplete = complete.replace(
            "\"completed_totals_incomplete\":false",
            "\"completed_totals_incomplete\":true",
        );
        assert!(parse(incomplete.as_bytes(), Profile::WalFull, true).is_err());
    }
}
