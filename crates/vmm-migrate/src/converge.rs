// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! When the pre-copy ends and the guest pauses.
//!
//! The final pass sends what the guest wrote while the last pre-copy
//! pass ran, and the guest is paused for all of it. So the passes run
//! back to back while each one shrinks, and the guest pauses once a
//! pass took no longer than the budget: the final pass is then about
//! that short too, at the bandwidth just measured.

use std::time::Duration;

/// The longest final pass the pause should hold.
pub(crate) const BUDGET: Duration = Duration::from_millis(50);

/// A guest that writes faster than the link carries never converges;
/// these bound the pre-copy for it.
pub(crate) const MAX_PASSES: u32 = 30;
const MAX_TIME: Duration = Duration::from_secs(30);
const STALLED_PASSES: u32 = 3;

#[derive(Debug)]
pub(crate) struct Precopy {
    passes: u32,
    spent: Duration,
    fewest: u64,
    stalled: u32,
}

impl Precopy {
    pub(crate) fn new() -> Self {
        Self {
            passes: 0,
            spent: Duration::ZERO,
            fewest: u64::MAX,
            stalled: 0,
        }
    }

    /// After a pass that sent `dirty` pages in `took`: true when the
    /// guest pauses now.
    pub(crate) fn done(&mut self, dirty: u64, took: Duration) -> bool {
        self.passes += 1;
        self.spent += took;
        if dirty == 0
            || took <= BUDGET
            || self.passes >= MAX_PASSES
            || self.spent >= MAX_TIME
        {
            return true;
        }
        // Less than a tenth fewer pages is no progress.
        if dirty < self.fewest.saturating_sub(self.fewest / 10) {
            self.fewest = dirty;
            self.stalled = 0;
        } else {
            self.stalled += 1;
        }
        self.stalled >= STALLED_PASSES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_millis(400);

    #[test]
    fn a_short_pass_ends_the_precopy() {
        let mut p = Precopy::new();
        assert!(!p.done(160_000, LONG));
        assert!(!p.done(25_000, Duration::from_millis(120)));
        assert!(p.done(6_000, BUDGET));
    }

    #[test]
    fn a_clean_pass_ends_the_precopy() {
        assert!(Precopy::new().done(0, LONG));
    }

    #[test]
    fn a_slow_small_pass_does_not_end_it() {
        let mut p = Precopy::new();
        assert!(!p.done(900, LONG));
        assert!(!p.done(300, LONG));
    }

    #[test]
    fn a_guest_that_outruns_the_link_pauses_after_three_stalls() {
        let mut p = Precopy::new();
        assert!(!p.done(50_000, LONG));
        assert!(!p.done(40_000, LONG));
        assert!(!p.done(45_000, LONG));
        assert!(!p.done(38_000, LONG));
        assert!(p.done(39_000, LONG));
    }

    #[test]
    fn the_passes_are_bounded() {
        let mut p = Precopy::new();
        let mut dirty = 1 << 40;
        let mut n = 1;
        while !p.done(dirty, Duration::from_millis(60)) {
            dirty /= 2;
            n += 1;
        }
        assert_eq!(n, MAX_PASSES);
    }

    #[test]
    fn the_time_is_bounded() {
        let mut p = Precopy::new();
        let mut dirty = 1 << 40;
        let mut n = 1;
        while !p.done(dirty, Duration::from_secs(4)) {
            dirty /= 2;
            n += 1;
        }
        assert_eq!(n, 8);
    }
}
