//! The active cleaner's progress check: it stalls when a window of victims frees no more blocks
//! than its cleaning appended, and pauses when it finds no victim it can start.

/// Victims per progress window. A victim's segment is credited only when a later publication
/// frees it, so a single victim can look unproductive. A window of eight smooths that out, and a
/// stall wastes at most eight victims of copies.
pub(crate) const ACTIVE_WINDOW_VICTIMS: u32 = 8;

/// Consecutive stalls double the pause, up to this many segments of user writes. A log whose
/// live data can never reach the stop threshold keeps the latch on; without the growing pause the
/// cleaner would spend eight victims of copies for every segment of user writes.
const MAXIMUM_PAUSE_SEGMENTS: u64 = 64;

/// The current window of active victims, and the pause a stall or an empty selection caused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActiveProgress {
    victims: u32,
    /// Segments freed by publications when the window opened; `None` before its first step.
    /// Every publication counts, whatever ran it: a checkpoint or a pressure step often frees
    /// an active victim. Segments that user overwrites emptied count too, which overstates the
    /// cleaner's gain; that errs toward running.
    freed_at_open: Option<u64>,
    /// Blocks that active steps appended in this window.
    appended_blocks: u64,
    /// Unproductive windows in a row.
    consecutive_stalls: u32,
    /// The cleaner pauses until the user payload block count reaches this.
    paused_until_user_blocks: u64,
}

impl ActiveProgress {
    /// Whether the window holds all its victims and waits for its verdict.
    pub(crate) fn window_full(&self) -> bool {
        self.victims >= ACTIVE_WINDOW_VICTIMS
    }

    /// Opens the window at its first step: frees counted from `freed_segments` on are credited.
    pub(crate) fn begin_step(&mut self, freed_segments: u64) {
        self.freed_at_open.get_or_insert(freed_segments);
    }

    /// Records what one active step appended and whether it started a victim.
    pub(crate) fn record_step(&mut self, appended_blocks: u64, started_victim: bool) {
        self.appended_blocks += appended_blocks;
        self.victims += u32::from(started_victim);
    }

    /// Closes a full window and opens the next one. Returns whether the window stalled the
    /// cleaner: publications since it opened freed no more blocks than its steps appended. Each
    /// consecutive stall doubles the pause, from one segment of user writes.
    pub(crate) fn close_window(
        &mut self,
        freed_segments: u64,
        segment_blocks: u64,
        user_blocks: u64,
    ) -> bool {
        let opened = self.freed_at_open.unwrap_or(freed_segments);
        let freed_blocks = (freed_segments - opened) * segment_blocks;
        let stalled = freed_blocks <= self.appended_blocks;
        let consecutive_stalls = if stalled {
            self.consecutive_stalls + 1
        } else {
            0
        };
        let mut paused_until_user_blocks = self.paused_until_user_blocks;
        if stalled {
            let doublings = (consecutive_stalls - 1).min(MAXIMUM_PAUSE_SEGMENTS.ilog2());
            paused_until_user_blocks = user_blocks + (1_u64 << doublings) * segment_blocks;
        }
        *self = Self {
            consecutive_stalls,
            paused_until_user_blocks,
            ..Self::default()
        };
        stalled
    }

    /// Pauses after a step found no victim it could start, until one segment of user writes:
    /// new overwrites can make a victim worth cleaning, and pressure cleaning can free the room
    /// one needs. Without the pause every tick would scan every segment for nothing.
    pub(crate) fn pause(&mut self, user_blocks: u64, segment_blocks: u64) {
        self.paused_until_user_blocks = self
            .paused_until_user_blocks
            .max(user_blocks + segment_blocks);
    }

    /// Whether the cleaner pauses at `user_blocks` user payload blocks written.
    pub(crate) fn paused(&self, user_blocks: u64) -> bool {
        user_blocks < self.paused_until_user_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEGMENT: u64 = 64;

    /// Runs one full window: `victims` steps that each start a victim and append `appended`,
    /// while publications free `freed_segments` in total. Returns whether it stalled.
    fn window(
        progress: &mut ActiveProgress,
        freed_segments: u64,
        appended: u64,
        user: u64,
    ) -> bool {
        let base = 1000;
        for _ in 0..ACTIVE_WINDOW_VICTIMS {
            progress.begin_step(base);
            progress.record_step(appended, true);
        }
        assert!(progress.window_full());
        progress.close_window(base + freed_segments, SEGMENT, user)
    }

    #[test]
    fn a_window_with_net_gain_keeps_the_cleaner_running() {
        let mut progress = ActiveProgress::default();
        assert!(!window(&mut progress, 8, 60, 0));
        assert!(!progress.paused(0));
        assert!(!progress.window_full(), "the next window opens empty");
    }

    #[test]
    fn frees_from_any_publication_after_the_window_opened_are_credited() {
        let mut progress = ActiveProgress::default();
        progress.begin_step(10);
        progress.record_step(500, true);
        // Steps after the first don't move the window's start.
        progress.begin_step(12);
        progress.record_step(10, false);
        // A checkpoint outside active steps freed segments 10 to 18: 512 blocks.
        assert!(
            !progress.close_window(18, SEGMENT, 0),
            "512 freed beat 510 appended"
        );
    }

    #[test]
    fn a_window_without_net_gain_pauses_for_a_segment_of_user_writes() {
        let mut progress = ActiveProgress::default();
        assert!(
            window(&mut progress, 8, 64, 100),
            "freed 512 blocks, appended 512"
        );
        assert!(progress.paused(100 + SEGMENT - 1));
        assert!(!progress.paused(100 + SEGMENT));
    }

    #[test]
    fn consecutive_stalls_double_the_pause_up_to_the_maximum() {
        let mut progress = ActiveProgress::default();
        for (stall, segments) in [1, 2, 4, 8, 16, 32, 64, 64].into_iter().enumerate() {
            assert!(window(&mut progress, 0, 1, 0), "stall {stall}");
            assert!(progress.paused(segments * SEGMENT - 1), "stall {stall}");
            assert!(!progress.paused(segments * SEGMENT), "stall {stall}");
        }
        assert!(!window(&mut progress, 8, 1, 0), "a productive window");
        assert!(window(&mut progress, 0, 1, 0));
        assert!(
            !progress.paused(SEGMENT),
            "the doubling restarted at one segment"
        );
    }

    #[test]
    fn an_empty_selection_pauses_for_one_segment_of_user_writes() {
        let mut progress = ActiveProgress::default();
        progress.pause(100, SEGMENT);
        assert!(progress.paused(100 + SEGMENT - 1));
        assert!(!progress.paused(100 + SEGMENT));
    }
}
