use super::{TemporaryBacking, format_segments};
use crate::{
    BLOCK_SIZE, Volume,
    accounted_io::fsync01::{Medium, Mode},
};
use std::{fs, io, path::Path};

// External history, independent of the backing and the engine's mapping or checksums.
const OLD: [u8; 2] = [0x10, 0x20];
const NEW: [u8; 2] = [0x31, 0x42];

fn write_pair(volume: &mut Volume, values: [u8; 2]) -> io::Result<()> {
    for (lba, value) in values.into_iter().enumerate() {
        volume.write_block(lba as u32, &[value; BLOCK_SIZE])?;
    }
    Ok(())
}

fn recovered_pair(path: &Path) -> io::Result<[[u8; BLOCK_SIZE]; 2]> {
    let mut copy = crate::open(path)?;
    let mut data = [[0; BLOCK_SIZE]; 2];
    for (lba, block) in data.iter_mut().enumerate() {
        copy.read_block(lba as u32, block)?;
    }
    Ok(data)
}

fn scenario(mode: Mode) -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 8, 12, 32)?;
    let mut volume = crate::open(&backing.0)?;
    volume.enable_records_flush_retry()?;
    write_pair(&mut volume, OLD)?;
    volume.flush()?;
    Medium::arm(&mut volume.backing_io, &backing.0, mode)?;
    write_pair(&mut volume, NEW)?;
    Medium::enter_client_flush(&mut volume.backing_io);
    let flush = volume.flush();
    let trace = Medium::take(&mut volume.backing_io);
    assert_eq!(
        trace.originals.len(),
        2,
        "fixture did not submit both data records"
    );
    assert_eq!(
        trace.lost,
        mode != Mode::Clean,
        "fixture missed the requested fault"
    );
    // Freeze the media cut before close, checkpoint or subsequent client I/O.
    let cut = TemporaryBacking::new()?;
    cut.open_new()?;
    fs::copy(&backing.0, &cut.0)?;
    let actual = recovered_pair(&cut.0)?;
    let history_matches = actual == NEW.map(|v| [v; BLOCK_SIZE]);
    if mode == Mode::Engine && flush.is_err() {
        eprintln!(
            "FSYNC-01 engine FLUSH={flush:?} latched={} recovery_matches_new={history_matches}",
            volume.is_failed()
        );
    }
    flush.expect("FSYNC-01 transient records barrier must resume within the same client FLUSH");
    assert!(
        trace.synced,
        "client success without successful backing barrier"
    );
    if mode == Mode::BareRetry {
        assert!(
            !history_matches,
            "history oracle accepted a bare fsync retry"
        );
        assert!(
            actual == OLD.map(|v| [v; BLOCK_SIZE]),
            "unexpected control image"
        );
        eprintln!(
            "FSYNC-01 planted FLUSH success rejected: both recovered blocks contain their old values"
        );
    } else {
        assert!(
            history_matches,
            "acknowledged data missing from recovered image"
        );
    }
    if mode == Mode::Engine {
        assert!(
            trace.replays == trace.originals,
            "missing, reordered or altered physical replay"
        );
    } else {
        assert!(trace.replays.is_empty(), "unexpected rewrite in control");
    }
    Ok(())
}

#[test]
fn fsync01_controls() -> io::Result<()> {
    scenario(Mode::Clean)?;
    scenario(Mode::BareRetry)
}

#[test]
fn fsync01_engine() -> io::Result<()> {
    scenario(Mode::Engine)
}
