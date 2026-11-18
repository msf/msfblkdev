//! SIGKILL at relocation, barrier and discard boundaries with a consistent-prefix oracle.
use super::*;

const HOT_LBAS: u32 = 16;
const COLD_FIRST: u32 = 16;
const COLD_LAST: u32 = 80;
const WRITES: u32 = 1200;
const RELOCATION_RECORD_WRITE_COMPLETE_FAILPOINT: &str = "relocation-record-write-complete";
const RELOCATION_MAPPING_INSTALLED_FAILPOINT: &str = "relocation-mapping-installed";

fn options() -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    }
}

fn value(index: u32) -> [u8; BLOCK_SIZE] {
    let mut block = [0; BLOCK_SIZE];
    for chunk in block.chunks_exact_mut(4) {
        chunk.copy_from_slice(&index.to_le_bytes());
    }
    block
}

/// The deterministic write sequence: hot LBAs churn, one cold LBA every twentieth write.
fn sequence() -> Vec<(u32, u32)> {
    let mut state = 0x1234_5678_u32;
    let mut cold = COLD_FIRST;
    (1..=WRITES)
        .map(|index| {
            let lba = if index % 20 == 0 && cold < COLD_LAST {
                cold += 1;
                cold - 1
            } else {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state % HOT_LBAS
            };
            (index, lba)
        })
        .collect()
}

/// Every recovered value must come from one prefix of the sequence: with `P` the largest
/// recovered index, each LBA must hold its last write at or before `P`.
fn verify_consistent_prefix(volume: &mut Volume) -> io::Result<u32> {
    let writes = sequence();
    let mut recovered = [0_u32; COLD_LAST as usize];
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, slot) in recovered.iter_mut().enumerate() {
        volume.read_block(lba as u32, &mut bytes)?;
        let index = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"));
        assert_eq!(bytes, value(index), "LBA {lba} holds a torn value");
        *slot = index;
    }
    let prefix = recovered.iter().copied().max().unwrap_or(0);
    for (lba, &index) in recovered.iter().enumerate() {
        let expected = writes
            .iter()
            .filter(|(write_index, write_lba)| *write_index <= prefix && *write_lba == lba as u32)
            .map(|(write_index, _)| *write_index)
            .next_back()
            .unwrap_or(0);
        assert_eq!(index, expected, "LBA {lba} disagrees with prefix {prefix}");
    }
    Ok(prefix)
}

#[test]
fn relocation_and_barrier_crashes_keep_a_consistent_prefix_and_a_usable_volume() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open_with_options(PathBuf::from(path), options())?;
        for (index, lba) in sequence() {
            volume.write_block(lba, &value(index))?;
            if index % 25 == 0 {
                volume.flush()?;
            }
        }
        return Err(io::Error::other(
            "reclamation failpoint did not pause the child",
        ));
    }
    let boundaries = [
        RELOCATION_RECORD_WRITE_COMPLETE_FAILPOINT,
        RELOCATION_MAPPING_INSTALLED_FAILPOINT,
        "segment-link-fsync-complete-relocation",
        "segment-link-write-complete-user",
        "backing-discard-complete",
        "checkpoint-records-fsync-complete",
        DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
        DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
    ];
    for boundary in boundaries {
        for _ in 0..configured_crash_test_plan(boundaries.len() as u64)?.repetitions {
            let backing = TemporaryBacking::new()?;
            format_segments(&backing, 128, 12, 64)?;
            sigkill_child_at_handshake(
                "tests::reclaim_crash::relocation_and_barrier_crashes_keep_a_consistent_prefix_and_a_usable_volume",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            let mut reopened = open_with_options(&backing.0, options())?;
            let prefix = verify_consistent_prefix(&mut reopened)?;
            assert!(prefix > 0, "{boundary}: nothing recovered");
            reopened.log.segments.check_conservation()?;
            // The volume keeps working after the interrupted maintenance.
            for (index, lba) in sequence().into_iter().take(200) {
                reopened.write_block(lba, &value(WRITES + index))?;
            }
            reopened.flush()?;
            let mut bytes = [0; BLOCK_SIZE];
            reopened.read_block(0, &mut bytes)?;
            let index = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"));
            assert!(index > WRITES, "{boundary}: later writes are visible");
            reopened.close()?;
        }
    }
    Ok(())
}
