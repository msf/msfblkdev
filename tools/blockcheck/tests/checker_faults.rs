//! The checker tests of `docs/BLOCKCHECK-PLAN.md`, "How do we test the checker?", run against
//! the simulated disk. Each deliberate-bug test names its trigger, LBA, violation kind and whether
//! detection happens before or after recovery, and checks that the fault occurred.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] functions; a failed unwrap is a failed test"
)]

use std::num::{NonZeroU32, NonZeroU64};

use blockcheck::checker::{Checker, Error, Report, Violation, ViolationKind};
use blockcheck::memory::{Fault, FaultPlan, Injected, MemoryTarget, Survival};
use blockcheck::payload::Content;
use blockcheck::rng::SplitMix64;
use blockcheck::target::IoFailure;

const BLOCKS: u64 = 16;
const SEED: u64 = 11;
const ALL_FAULTS: [Fault; 7] = [
    Fault::StaleRead,
    Fault::MisdirectedWrite,
    Fault::IgnoredDiscard,
    Fault::LostFlush,
    Fault::UnflushedDiscard,
    Fault::Resurrection,
    Fault::RejectedWriteLands,
];

fn fixture(plan: Option<FaultPlan>) -> (MemoryTarget, Checker) {
    let target = MemoryTarget::new(BLOCKS, plan, SEED).unwrap();
    (target, Checker::new(SEED, BLOCKS).unwrap())
}

const fn from_start(fault: Fault) -> FaultPlan {
    FaultPlan {
        fault,
        from_request: 0,
    }
}

fn version(value: u32) -> Content {
    Content::Version(NonZeroU32::new(value).unwrap())
}

fn assert_only(checker: &Checker, expected: Violation) {
    assert_eq!(checker.report().violations, [expected]);
    assert_eq!(checker.report().violation_count, 1);
}

/// After recovery rewrote and flushed, every LBA must read back as the model expects.
fn assert_known_state(target: &mut MemoryTarget, checker: &mut Checker) {
    let before = checker.report().violation_count;
    for lba in 0..BLOCKS {
        checker.read(target, lba).unwrap();
    }
    assert_eq!(checker.report().violation_count, before);
}

/// Writes, flushes, discards, a rejection, an unflushed overwrite, a crash and a recovery.
fn scripted(target: &mut MemoryTarget, checker: &mut Checker, survival: Survival) {
    for lba in 0..BLOCKS {
        checker.write(target, lba).unwrap();
    }
    checker.flush(target).unwrap();
    checker.discard(target, 2, 3).unwrap();
    checker.write_zero(target, 8).unwrap();
    target.inject(Injected::NoSpace);
    assert!(matches!(
        checker.write(target, 9),
        Err(Error::Io(IoFailure::NoSpace))
    ));
    checker.write(target, 9).unwrap();
    for lba in 0..BLOCKS {
        checker.read(target, lba).unwrap();
    }
    target.crash(survival);
    checker.recover(target).unwrap();
    assert_known_state(target, checker);
}

#[test]
fn correct_target_passes_before_and_after_recovery() {
    for survival in [Survival::KeepAll, Survival::DropAll, Survival::Random] {
        let (mut target, mut checker) = fixture(None);
        scripted(&mut target, &mut checker, survival);
        assert_eq!(checker.report().violation_count, 0, "{survival:?}");
    }
}

#[test]
fn durable_data_lost_after_recovery_is_detected() {
    // Trigger: flushes persist nothing. LBA 5: v1 flushed, crash returns the zeros from before v1.
    let (mut target, mut checker) = fixture(Some(from_start(Fault::LostFlush)));
    checker.write(&mut target, 5).unwrap();
    checker.flush(&mut target).unwrap();
    checker.read(&mut target, 5).unwrap();
    assert_eq!(
        checker.report().violation_count,
        0,
        "no detection before recovery"
    );
    target.crash(Survival::DropAll);
    checker.recover(&mut target).unwrap();
    assert!(target.faults_occurred() > 0);
    assert_only(
        &checker,
        Violation {
            lba: 5,
            expected: version(1),
            found: Content::Zero,
            kind: ViolationKind::LostData,
            after_crash: true,
        },
    );
    assert_known_state(&mut target, &mut checker);
}

#[test]
fn data_from_another_lba_is_detected_without_a_crash() {
    // Trigger: writes land on the neighbouring LBA. A write to LBA 2 is read back from LBA 3.
    let (mut target, mut checker) = fixture(Some(from_start(Fault::MisdirectedWrite)));
    checker.write(&mut target, 2).unwrap();
    checker.read(&mut target, 3).unwrap();
    assert!(target.faults_occurred() > 0);
    assert_only(
        &checker,
        Violation {
            lba: 3,
            expected: Content::Zero,
            found: Content::Invalid,
            kind: ViolationKind::Corrupt,
            after_crash: false,
        },
    );
}

#[test]
fn discarded_data_returning_after_recovery_is_detected() {
    // Trigger: flushes keep discards cached. LBA 7: v1 flushed, discarded and flushed, then the
    // crash drops the discard and v1 returns.
    let (mut target, mut checker) = fixture(Some(from_start(Fault::UnflushedDiscard)));
    checker.write(&mut target, 7).unwrap();
    checker.flush(&mut target).unwrap();
    checker.discard(&mut target, 7, 1).unwrap();
    checker.flush(&mut target).unwrap();
    checker.read(&mut target, 7).unwrap();
    assert_eq!(
        checker.report().violation_count,
        0,
        "no detection before recovery"
    );
    target.crash(Survival::DropAll);
    checker.recover(&mut target).unwrap();
    assert!(target.faults_occurred() > 0);
    assert_only(
        &checker,
        Violation {
            lba: 7,
            expected: Content::Zero,
            found: version(1),
            kind: ViolationKind::Stale,
            after_crash: true,
        },
    );
    assert_known_state(&mut target, &mut checker);
}

#[test]
fn rejected_write_becoming_visible_is_detected_after_recovery() {
    // Trigger: a write rejected with NoSpace reaches the medium. LBA 9: v1 flushed, v2 rejected,
    // v3 written, then the crash drops v3 and the target returns v2.
    for plan in [None, Some(from_start(Fault::RejectedWriteLands))] {
        let (mut target, mut checker) = fixture(plan);
        checker.write(&mut target, 9).unwrap();
        checker.flush(&mut target).unwrap();
        target.inject(Injected::NoSpace);
        assert!(matches!(
            checker.write(&mut target, 9),
            Err(Error::Io(IoFailure::NoSpace))
        ));
        checker.write(&mut target, 9).unwrap();
        target.crash(Survival::DropAll);
        checker.recover(&mut target).unwrap();
        assert_known_state(&mut target, &mut checker);
        if plan.is_none() {
            assert_eq!(checker.report().violation_count, 0, "control run");
            continue;
        }
        assert!(target.faults_occurred() > 0);
        assert_only(
            &checker,
            Violation {
                lba: 9,
                expected: version(3),
                found: version(2),
                kind: ViolationKind::NeverAcknowledged,
                after_crash: true,
            },
        );
    }
}

#[test]
fn uncertain_write_is_accepted_when_it_survives() {
    // LBA 4: v1 flushed, v2 fails with an I/O error after it reached the cache. Before the crash
    // and after it, v1 and v2 are both permitted.
    for survival in [Survival::KeepAll, Survival::DropAll] {
        let (mut target, mut checker) = fixture(None);
        checker.write(&mut target, 4).unwrap();
        checker.flush(&mut target).unwrap();
        target.inject(Injected::IoErrorApplied);
        assert!(matches!(
            checker.write(&mut target, 4),
            Err(Error::Io(IoFailure::Other(_)))
        ));
        checker.read(&mut target, 4).unwrap();
        target.crash(survival);
        checker.recover(&mut target).unwrap();
        assert_eq!(checker.report().violation_count, 0, "{survival:?}");
        assert_eq!(checker.report().uncertain, 1);
        assert_known_state(&mut target, &mut checker);
    }
}

#[test]
fn recovery_reads_every_lba_before_rewriting() {
    // Requests: write, flush, write, then one read per LBA. The fault starts at the first repair:
    // the rewrite of LBA 2 lands on LBA 3. Interleaved repair would read it as LBA 3's contents.
    let plan = FaultPlan {
        fault: Fault::MisdirectedWrite,
        from_request: 3 + BLOCKS,
    };
    let (mut target, mut checker) = fixture(Some(plan));
    checker.write(&mut target, 2).unwrap();
    checker.flush(&mut target).unwrap();
    checker.write(&mut target, 2).unwrap();
    target.crash(Survival::DropAll);
    checker.recover(&mut target).unwrap();
    assert_eq!(target.faults_occurred(), 1);
    assert_eq!(checker.report().repairs, 1);
    assert_eq!(
        checker.report().violation_count,
        0,
        "verdicts precede rewrites"
    );
    checker.read(&mut target, 3).unwrap();
    assert_eq!(checker.report().violations[0].kind, ViolationKind::Corrupt);
}

#[test]
fn uncertain_write_that_recovery_retired_cannot_return_later() {
    // Trigger: a crash restores each block's first contents, from request 4 (the second crash).
    // LBA 6: v1 fails uncertainly and the crash drops it; recovery reads zeros. v2 is written,
    // then the second crash restores v1, which recovery had already ruled out.
    let plan = FaultPlan {
        fault: Fault::Resurrection,
        from_request: 2 + BLOCKS,
    };
    let (mut target, mut checker) = fixture(Some(plan));
    target.inject(Injected::IoErrorApplied);
    assert!(checker.write(&mut target, 6).is_err());
    target.crash(Survival::DropAll);
    checker.recover(&mut target).unwrap();
    assert_eq!(checker.report().violation_count, 0);
    checker.write(&mut target, 6).unwrap();
    target.crash(Survival::DropAll);
    checker.recover(&mut target).unwrap();
    assert!(target.faults_occurred() > 0);
    assert_only(
        &checker,
        Violation {
            lba: 6,
            expected: version(2),
            found: version(1),
            kind: ViolationKind::NeverAcknowledged,
            after_crash: true,
        },
    );
}

#[test]
fn fault_scheduled_after_the_last_request_never_occurs() {
    for fault in ALL_FAULTS {
        let plan = Some(FaultPlan {
            fault,
            from_request: u64::MAX,
        });
        let (mut target, mut checker) = fixture(plan);
        scripted(&mut target, &mut checker, Survival::DropAll);
        assert_eq!(checker.report().violation_count, 0, "{fault:?}");
        assert_eq!(target.faults_occurred(), 0, "{fault:?}");
    }
}

const RANDOM_BLOCKS: NonZeroU64 = NonZeroU64::new(64).unwrap();
const PERCENT: NonZeroU64 = NonZeroU64::new(100).unwrap();
const RANDOM_OPERATIONS: u32 = 20_000;
const MAX_DISCARD_BLOCKS: u64 = 4;

/// Returns the checker report and how often the fault occurred.
fn run_random(seed: u64, plan: Option<FaultPlan>) -> Result<(Report, u64), Error> {
    let blocks = RANDOM_BLOCKS.get();
    let mut target = MemoryTarget::new(blocks, plan, seed)
        .map_err(|error| Error::Io(IoFailure::Other(error)))?;
    let mut checker = Checker::new(seed, blocks)?;
    let mut rng = SplitMix64::new(seed ^ 0x5eed);
    for _ in 0..RANDOM_OPERATIONS {
        let lba = rng.below(RANDOM_BLOCKS);
        let result = match rng.below(PERCENT) {
            0..40 => checker.write(&mut target, lba),
            40..73 => checker.read(&mut target, lba),
            73..83 => checker.flush(&mut target),
            83..90 => {
                let blocks = blocks.saturating_sub(lba).min(MAX_DISCARD_BLOCKS);
                checker.discard(&mut target, lba, blocks)
            }
            90..94 => checker.write_zero(&mut target, lba),
            94..96 => {
                target.inject(match lba % 3 {
                    0 => Injected::NoSpace,
                    1 => Injected::IoErrorApplied,
                    _ => Injected::IoErrorDropped,
                });
                Ok(())
            }
            _ => Err(Error::Io(IoFailure::Other(std::io::Error::other("crash")))),
        };
        match result {
            Ok(()) | Err(Error::Io(IoFailure::NoSpace)) => {}
            Err(Error::Io(IoFailure::Other(_))) => {
                target.crash(Survival::Random);
                checker.recover(&mut target)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok((checker.report().clone(), target.faults_occurred()))
}

#[test]
fn random_runs_pass_on_a_correct_target() {
    for seed in 1..=8 {
        let (report, occurred) = run_random(seed, None).unwrap();
        assert_eq!(occurred, 0);
        assert_eq!(
            report.violation_count, 0,
            "seed {seed}: {:?}",
            report.violations
        );
    }
}

#[test]
fn random_runs_detect_each_fault() {
    for fault in ALL_FAULTS {
        let (report, occurred) = run_random(3, Some(from_start(fault))).unwrap();
        assert!(occurred > 0, "{fault:?} never occurred");
        assert!(
            report.violation_count > 0,
            "{fault:?} occurred {occurred} times, not detected"
        );
    }
}

#[test]
fn rejects_invalid_geometry_and_ranges() {
    assert!(matches!(
        Checker::new(1, 0),
        Err(Error::InvalidBlockCount(0))
    ));
    assert!(Checker::new(1, blockcheck::checker::MAX_BLOCKS + 1).is_err());
    let (mut target, mut checker) = fixture(None);
    assert!(matches!(
        checker.write(&mut target, BLOCKS),
        Err(Error::LbaOutOfRange { .. })
    ));
    assert!(matches!(
        checker.discard(&mut target, 0, 0),
        Err(Error::InvalidRange { .. })
    ));
    assert!(matches!(
        checker.discard(&mut target, BLOCKS - 1, 2),
        Err(Error::InvalidRange { .. })
    ));
    let mut other = MemoryTarget::new(BLOCKS / 2, None, 1).unwrap();
    assert!(matches!(
        checker.recover(&mut other),
        Err(Error::GeometryMismatch { .. })
    ));
}

#[test]
fn second_uncertain_write_before_recovery_marks_the_run_incomplete() {
    let (mut target, mut checker) = fixture(None);
    target.inject(Injected::IoErrorDropped);
    assert!(checker.write(&mut target, 1).is_err());
    target.inject(Injected::IoErrorDropped);
    assert!(matches!(
        checker.write(&mut target, 1),
        Err(Error::UncertainLimit { lba: 1 })
    ));
}
