use super::*;

pub(super) const REPETITIONS: u8 = 3;
pub(super) const MATRIX_CASES: usize = 29;
pub(super) const MATRIX_RUNS: usize = MATRIX_CASES * REPETITIONS as usize;

pub(super) const PROFILES: [Profile; 3] =
    [Profile::DeleteExtra, Profile::WalFull, Profile::WalNormal];

pub(super) const SHORT_CASES: [Case; 9] = [
    Case::SpeedtestMain,
    Case::KvSequentialReadSmall,
    Case::KvRandomReadSmall,
    Case::KvSequentialReadLarge,
    Case::KvRandomReadLarge,
    Case::KvSequentialUpdateSmallSingleTransaction,
    Case::KvSequentialUpdateSmallPerOpCommit,
    Case::KvRandomUpdateLargeSingleTransaction,
    Case::KvRandomUpdateLargePerOpCommit,
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Selection {
    pub(super) case: Case,
    pub(super) profile: Profile,
    pub(super) repetition: u8,
}

pub(super) fn full() -> Vec<Selection> {
    let mut selections = Vec::with_capacity(MATRIX_RUNS);
    for profile in PROFILES {
        for case in SHORT_CASES {
            push_repetitions(&mut selections, case, profile);
        }
    }
    assert_eq!(selections.len(), 81, "all short selections must run first");
    for profile in [Profile::WalFull, Profile::WalNormal] {
        push_repetitions(&mut selections, Case::KvLongWalCheckpointStress, profile);
    }
    assert_eq!(selections.len(), MATRIX_RUNS, "SQLite matrix size changed");
    selections
}

fn push_repetitions(selections: &mut Vec<Selection>, case: Case, profile: Profile) {
    for repetition in 1..=REPETITIONS {
        selections.push(Selection {
            case,
            profile,
            repetition,
        });
    }
}

pub(super) fn selected(case: Case, profile: Profile, repetition: u8) -> io::Result<Selection> {
    if !(1..=REPETITIONS).contains(&repetition) || !allowed(case, profile) {
        return Err(invalid_selection("case/profile/repetition"));
    }
    Ok(Selection {
        case,
        profile,
        repetition,
    })
}

fn allowed(case: Case, profile: Profile) -> bool {
    case != Case::KvLongWalCheckpointStress
        || matches!(profile, Profile::WalFull | Profile::WalNormal)
}

fn invalid_selection(field: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid SQLite matrix {field} selector"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn full_matrix_is_exact_and_unique() {
        let matrix = full();
        assert_eq!(MATRIX_CASES, 29);
        assert_eq!(MATRIX_RUNS, 87);
        assert_eq!(matrix.len(), 87);
        let unique: BTreeSet<_> = matrix
            .iter()
            .map(|item| (item.case.name(), item.profile.name(), item.repetition))
            .collect();
        assert_eq!(unique.len(), matrix.len());
        assert_eq!(
            matrix
                .iter()
                .filter(|item| item.case == Case::KvLongWalCheckpointStress)
                .count(),
            6
        );
        assert!(
            matrix[..81]
                .iter()
                .all(|item| item.case != Case::KvLongWalCheckpointStress)
        );
        assert!(
            matrix[81..]
                .iter()
                .all(|item| item.case == Case::KvLongWalCheckpointStress)
        );
    }

    #[test]
    fn selector_accepts_only_matrix_members() {
        assert!(selected(Case::SpeedtestMain, Profile::DeleteExtra, 1).is_ok());
        assert!(selected(Case::KvLongWalCheckpointStress, Profile::WalNormal, 3).is_ok());
        assert!(selected(Case::KvLongWalCheckpointStress, Profile::DeleteExtra, 1).is_err());
        assert!(selected(Case::SpeedtestMain, Profile::WalFull, 0).is_err());
    }
}
