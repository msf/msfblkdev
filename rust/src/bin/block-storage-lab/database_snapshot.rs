use crate::config::DatabaseSnapshotRequest;
use crate::database_io::{self, BlockCounters};
use block_storage::io_accounting::{Reader, Snapshot};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const LEDGER_ENV: &str = "BLOCK_STORAGE_IO_LEDGER";
const DEVICE_ENV: &str = "BLOCK_STORAGE_IO_DEVICE_ID";
const MAX_STAT_BYTES: u64 = 4096;

struct Inputs {
    ledger: PathBuf,
    device_id: u32,
}

pub(crate) fn run(request: DatabaseSnapshotRequest) -> io::Result<()> {
    let inputs = inputs_from_env(|name| std::env::var_os(name))?;
    // SAFETY: this internal command is invoked only while its caller owns the
    // private ledger inode and promises not to replace, truncate, or mutate it
    // outside io_accounting until this short-lived reader is dropped.
    let reader = unsafe { Reader::open(&inputs.ledger)? };
    let ledger = reader.snapshot()?;
    let block = read_block_stat(inputs.device_id)?;
    let monotonic_ns = monotonic_ns()?;
    let record = record(request, monotonic_ns, &block, &ledger);
    let line = serde_json::to_string(&record).map_err(io::Error::other)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "sqlite-lab {line}")?;
    output.flush()
}

fn inputs_from_env<F>(mut get: F) -> io::Result<Inputs>
where
    F: FnMut(&str) -> Option<OsString>,
{
    let ledger = get(LEDGER_ENV).ok_or_else(|| invalid_input("missing BLOCK_STORAGE_IO_LEDGER"))?;
    if ledger.is_empty() {
        return Err(invalid_input("empty BLOCK_STORAGE_IO_LEDGER"));
    }
    let device =
        get(DEVICE_ENV).ok_or_else(|| invalid_input("missing BLOCK_STORAGE_IO_DEVICE_ID"))?;
    let device_id = parse_device_id(&device)?;
    Ok(Inputs {
        ledger: PathBuf::from(ledger),
        device_id,
    })
}

fn parse_device_id(value: &OsStr) -> io::Result<u32> {
    let text = value
        .to_str()
        .ok_or_else(|| invalid_input("invalid BLOCK_STORAGE_IO_DEVICE_ID"))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_input("invalid BLOCK_STORAGE_IO_DEVICE_ID"));
    }
    text.parse()
        .map_err(|_| invalid_input("invalid BLOCK_STORAGE_IO_DEVICE_ID"))
}

fn read_block_stat(device_id: u32) -> io::Result<BlockCounters> {
    let path = Path::new("/sys/block")
        .join(format!("ublkb{device_id}"))
        .join("stat");
    let mut bytes = Vec::with_capacity(256);
    File::open(path)?
        .take(MAX_STAT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(io::Error::other)? > MAX_STAT_BYTES {
        return Err(io::Error::other("block stat exceeds size limit"));
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    database_io::parse_block_stat(text)
}

fn monotonic_ns() -> io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to writable storage and CLOCK_MONOTONIC is a valid clock.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seconds = u64::try_from(time.tv_sec).map_err(|_| io::Error::other("negative clock"))?;
    let nanos = u64::try_from(time.tv_nsec).map_err(|_| io::Error::other("negative clock"))?;
    if nanos >= 1_000_000_000 {
        return Err(io::Error::other("invalid monotonic clock"));
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or_else(|| io::Error::other("monotonic clock overflow"))
}

fn record(
    request: DatabaseSnapshotRequest,
    monotonic_ns: u64,
    block: &BlockCounters,
    ledger: &Snapshot,
) -> Value {
    json!({
        "record": "device_snapshot",
        "phase": request.phase.as_str(),
        "connection": request.connection,
        "monotonic_ns": monotonic_ns,
        "block": {
            "reads": block.reads,
            "writes": block.writes,
            "flushes": block.flushes,
        },
        "ledger": {
            "write_limit_bytes": ledger.write_limit_bytes,
            "write_reserved_bytes": ledger.write_reserved_bytes,
            "write_completed_bytes": ledger.write_completed_bytes,
            "read_completed_bytes": ledger.read_completed_bytes,
            "read_completed_operations": ledger.read_completed_operations,
            "write_completed_operations": ledger.write_completed_operations,
            "flush_completed_operations": ledger.flush_completed_operations,
            "pending_operations": ledger.pending_operations,
            "errors": ledger.errors,
            "completed_totals_incomplete": ledger.completed_totals_incomplete,
            "budget_exhausted": ledger.budget_exhausted,
        }
    })
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseSnapshotPhase;
    use block_storage::io_accounting::Reader;
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn validates_snapshot_environment() {
        let missing = HashMap::<&str, OsString>::new();
        assert!(inputs_from_env(|name| missing.get(name).cloned()).is_err());
        for device in ["", "-1", "+1", "1.0", "4294967296"] {
            let values = HashMap::from([
                (LEDGER_ENV, OsString::from("/private/ledger")),
                (DEVICE_ENV, OsString::from(device)),
            ]);
            assert!(inputs_from_env(|name| values.get(name).cloned()).is_err());
        }
        let values = HashMap::from([
            (LEDGER_ENV, OsString::from("/private/ledger")),
            (DEVICE_ENV, OsString::from("17")),
        ]);
        let inputs = inputs_from_env(|name| values.get(name).cloned()).unwrap();
        assert_eq!(inputs.device_id, 17);
        assert_eq!(inputs.ledger, Path::new("/private/ledger"));
    }

    #[test]
    fn snapshot_json_preserves_reader_fields() {
        let path = std::env::temp_dir().join(format!(
            "block-storage-snapshot-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // SAFETY: this test owns the inode and removes it only after the reader is dropped.
        let reader = unsafe { Reader::create(&path, 8192) }.unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        let ledger = reader.snapshot().unwrap();
        let value = record(
            DatabaseSnapshotRequest {
                phase: DatabaseSnapshotPhase::DrainEnd,
                connection: 64,
            },
            123,
            &BlockCounters {
                reads: 1,
                writes: 2,
                flushes: 3,
            },
            &ledger,
        );
        assert_eq!(value["record"], "device_snapshot");
        assert_eq!(value["phase"], "drain_end");
        assert_eq!(value["connection"], 64);
        assert_eq!(value["block"]["writes"], 2);
        assert_eq!(value["ledger"]["write_limit_bytes"], 8192);
        assert_eq!(value["ledger"]["pending_operations"], 0);
        assert_eq!(value["ledger"]["completed_totals_incomplete"], false);
        drop(reader);
        fs::remove_file(path).unwrap();
    }
}
