//! The active cleaner's progress check: it stalls when a window of victims frees no more
//! blocks than its cleaning appended.

/// Victims per progress window. Each victim's gain is credited only after its barrier
/// publications, and one publication can free several victims, so a single victim can look
/// unproductive. A window of eight smooths that out, and a stall wastes at most eight victims of
/// copies.
pub(crate) const ACTIVE_WINDOW_VICTIMS: u32 = 8;

/// Cleaning output of one window, and the stall it may have caused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActiveProgress {
    victims: u32,
    freed_blocks: u64,
    appended_blocks: u64,
    /// User payload blocks written when the cleaner stalled; `None` when it never stalled.
    stalled_at_user_blocks: Option<u64>,
}

impl ActiveProgress {
    /// Records one active maintenance step: the blocks its publications freed, the blocks it
    /// appended, and whether it started a victim. Returns whether the step closed a window
    /// without net gain, which stalls the cleaner at `user_blocks`.
    pub(crate) fn record(
        &mut self,
        freed_blocks: u64,
        appended_blocks: u64,
        started_victim: bool,
        user_blocks: u64,
    ) -> bool {
        self.freed_blocks += freed_blocks;
        self.appended_blocks += appended_blocks;
        self.victims += u32::from(started_victim);
        if self.victims < ACTIVE_WINDOW_VICTIMS {
            return false;
        }
        let stalled = self.freed_blocks <= self.appended_blocks;
        let stalled_at_user_blocks = if stalled {
            Some(user_blocks)
        } else {
            self.stalled_at_user_blocks
        };
        *self = Self {
            stalled_at_user_blocks,
            ..Self::default()
        };
        stalled
    }

    /// Whether the cleaner is still stalled: fewer than `resume_blocks` user payload blocks were
    /// written since the stall. New overwrites create the dead space that can make victims
    /// worth cleaning again.
    pub(crate) fn stalled(&self, user_blocks: u64, resume_blocks: u64) -> bool {
        self.stalled_at_user_blocks
            .is_some_and(|at| user_blocks < at + resume_blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records `victims` steps that each start a victim, free `freed` and append `appended`.
    fn record_victims(progress: &mut ActiveProgress, victims: u32, freed: u64, appended: u64) {
        for _ in 0..victims {
            progress.record(freed, appended, true, 0);
        }
    }

    #[test]
    fn a_window_with_net_gain_keeps_the_cleaner_running() {
        let mut progress = ActiveProgress::default();
        record_victims(&mut progress, ACTIVE_WINDOW_VICTIMS - 1, 64, 60);
        assert!(!progress.record(64, 60, true, 0));
        assert!(!progress.stalled(0, 64));
    }

    #[test]
    fn one_bad_victim_in_a_productive_window_does_not_stall() {
        let mut progress = ActiveProgress::default();
        progress.record(0, 64, true, 0);
        record_victims(&mut progress, ACTIVE_WINDOW_VICTIMS - 1, 64, 50);
        assert!(!progress.stalled(0, 64));
    }

    #[test]
    fn a_window_without_net_gain_stalls_until_a_segment_of_user_writes() {
        let mut progress = ActiveProgress::default();
        record_victims(&mut progress, ACTIVE_WINDOW_VICTIMS - 1, 64, 64);
        // Publication steps between victims count toward the window without closing it.
        assert!(!progress.record(0, 3, false, 100));
        assert!(progress.record(64, 64, true, 100), "freed equals appended");
        assert!(progress.stalled(163, 64));
        assert!(
            !progress.stalled(164, 64),
            "one segment of user writes later"
        );
    }

    #[test]
    fn a_productive_window_after_a_stall_keeps_the_stall_point() {
        let mut progress = ActiveProgress::default();
        record_victims(&mut progress, ACTIVE_WINDOW_VICTIMS, 0, 64);
        assert!(progress.stalled(0, 64));
        record_victims(&mut progress, ACTIVE_WINDOW_VICTIMS, 64, 1);
        assert!(progress.stalled(0, 64), "only user writes end a stall");
    }
}
