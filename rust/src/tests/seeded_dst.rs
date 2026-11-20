//! Bounded serialized format-4 recovery runner. No scheduler or clock selects faults.
mod plan;
use super::{FormatOptions, TemporaryBacking};
use crate::{
    BLOCK_SIZE, BackingIo, Volume, accounted_io::fault_plan::FaultPlan, format_v4::storage,
};
use plan::{Effect::*, Epoch, Plan};
use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::atomic::Ordering,
};

const LBAS: usize = 8;
const LAST: u32 = LBAS as u32 - 1;

fn image() -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    // At most 24 two-block records, one sentinel and the format-time table, no cleaner.
    backing.create_sized((2 + 12 * 64) * BLOCK_SIZE as u64)?;
    storage::format_with_io(
        &backing.0,
        (LBAS * BLOCK_SIZE) as u64,
        FormatOptions {
            blocks_per_segment: 64,
        },
        || Ok(0x44535401),
        || BackingIo::new(None),
    )?;
    Ok(backing)
}

fn compare(volume: &mut Volume, expected: &[u8; LBAS]) -> io::Result<Option<usize>> {
    let mut block = [0; BLOCK_SIZE];
    for (lba, &value) in expected.iter().enumerate() {
        volume.read_block(lba as u32, &mut block)?;
        if block != [value; BLOCK_SIZE] {
            return Ok(Some(lba));
        }
    }
    Ok(None)
}

fn execute_epoch(
    volume: &mut Volume,
    epoch: &Epoch,
    expected: &mut [u8; LBAS],
    trace: &mut dyn Write,
) -> io::Result<()> {
    let (opcode, action) = epoch.effect.fault();
    let hits = FaultPlan::install(&mut volume.backing_io, opcode, epoch.ordinal, action);
    for (i, &(lba, value)) in epoch.writes.iter().enumerate() {
        let write = volume.write_block(lba, &[value; BLOCK_SIZE]);
        let (site, result) = match write {
            Ok(()) => {
                writeln!(trace, "write lba={lba} value={value} ack")?;
                ("sync", volume.flush())
            }
            Err(err) => ("write", Err(err)),
        };
        if let Err(err) = result {
            let count = hits.load(Ordering::Relaxed);
            if count != 1 {
                return Err(io::Error::other(format!(
                    "INCONCLUSIVE: error before injection: {err}"
                )));
            }
            let (wanted_site, wanted_errno) = match epoch.effect {
                RejectWrite => ("write", Some(libc::ENOSPC)),
                ShortWrite => ("write", None),
                CompletedWrite => ("write", Some(libc::EIO)),
                RejectSync | CompletedSync => ("sync", Some(libc::EIO)),
            };
            if i + 1 != epoch.ordinal
                || site != wanted_site
                || err.raw_os_error() != wanted_errno
                || (wanted_errno.is_none() && err.to_string() != "short io_uring operation")
            {
                return Err(io::Error::other(format!(
                    "unexpected client outcome: hits={count} {site} ordinal={} error={err}",
                    i + 1
                )));
            }
            if !volume.is_failed() || volume.flush().is_ok() {
                return Err(io::Error::other("failed volume did not fence"));
            }
            // This regular-file cut preserves completed writes, including the write
            // before a failed sync. No bytes were submitted for reject/short Writev.
            if !matches!(epoch.effect, RejectWrite | ShortWrite) {
                expected[lba as usize] = value;
            }
            writeln!(
                trace,
                "hit={count} {site} ordinal={} lba={lba} value={value} errno={wanted_errno:?} fenced",
                i + 1
            )?;
            return Ok(());
        }
        if hits.load(Ordering::Relaxed) != 0 {
            return Err(io::Error::other(
                "client falsely acknowledged injected fault",
            ));
        }
        writeln!(trace, "flush ack")?;
        expected[lba as usize] = value;
    }
    Err(io::Error::other("INCONCLUSIVE: requested injection missed"))
}

fn run(plan: &Plan, misdirect: bool, trace: &mut dyn Write) -> io::Result<Option<usize>> {
    let mut backing = image()?;
    let mut volume = crate::open(&backing.0)?;
    let mut history = [0; LBAS];
    // Misdirection is a harness control, never part of the oracle's history.
    volume.write_block(if misdirect { LAST } else { LAST - 1 }, &[0xa7; BLOCK_SIZE])?;
    volume.flush()?;
    history[LAST as usize - 1] = 0xa7;
    for (i, epoch) in plan.0.iter().enumerate() {
        writeln!(trace, "epoch={i}")?;
        execute_epoch(&mut volume, epoch, &mut history, trace)?;
        let cut = TemporaryBacking::new()?;
        cut.open_new()?;
        fs::copy(&backing.0, &cut.0)?;
        drop(volume);
        volume = crate::open(&cut.0)?;
        if let Some(lba) = compare(&mut volume, &history)? {
            writeln!(trace, "FAIL recovery lba={lba}")?;
            return Ok(Some(lba));
        }
        writeln!(trace, "recovered all {LBAS} blocks: {history:?}")?;
        backing = cut;
    }
    writeln!(trace, "PASS epochs={}", plan.0.len())?;
    Ok(None)
}

fn invocation_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("DST_ROOT").unwrap_or_default())
}

fn from_env() -> io::Result<Plan> {
    match (std::env::var_os("SEED"), std::env::var_os("PLAN")) {
        (None, Some(path)) => {
            let file = fs::File::open(invocation_dir().join(path))?;
            let mut text = String::new();
            // v1 needs less than 1 KiB even at maximum workload length.
            if !file.metadata()?.is_file() || file.take(1025).read_to_string(&mut text)? > 1024 {
                return Err(io::Error::other("expected a plan file <= 1024 bytes"));
            }
            plan::parse(&text)
        }
        (Some(seed), None) => Ok(plan::generate(
            seed.to_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| io::Error::other("SEED must be a decimal u64"))?,
        )),
        _ => Err(io::Error::other("set exactly one of SEED or PLAN")),
    }
}

#[test]
#[ignore = "opt-in plan from SEED or PLAN; use make test-fault-seed"]
fn replay_from_environment() -> io::Result<()> {
    let plan = from_env()?;
    let mut trace: Box<dyn Write> = if let Some(out) = std::env::var_os("OUT") {
        let dir = invocation_dir().join(out);
        fs::create_dir(&dir)?;
        fs::File::create_new(dir.join("plan.txt"))?.write_all(plan.to_string().as_bytes())?;
        Box::new(fs::File::create_new(dir.join("trace.txt"))?)
    } else {
        Box::new(io::stdout())
    };
    writeln!(trace, "{plan}")?;
    match run(&plan, false, &mut trace) {
        Ok(None) => Ok(()),
        Ok(Some(lba)) => Err(io::Error::other(format!("history mismatch at LBA {lba}"))),
        Err(e) => {
            writeln!(trace, "ABORT: {e}")?;
            Err(e)
        }
    }
}

#[test]
fn generated_and_replayed_histories_match_and_reject_misdirection() -> io::Result<()> {
    let p = plan::generate(42);
    let restored = plan::parse(&p.to_string())?;
    let (mut first, mut second) = (Vec::new(), Vec::new());
    assert_eq!(run(&p, false, &mut first)?, None);
    assert_eq!(run(&restored, false, &mut second)?, None);
    assert_eq!(first, second);
    assert_eq!(
        run(&restored, true, &mut io::sink())?,
        Some((LAST - 1) as usize)
    );
    let miss = plan::parse("format4-dst-v1\nsync-eio 2 1:5")?;
    assert_eq!(
        run(&miss, false, &mut io::sink()).unwrap_err().to_string(),
        "INCONCLUSIVE: requested injection missed"
    );
    Ok(())
}
