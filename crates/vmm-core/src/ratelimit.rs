// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A token bucket for events a guest or a peer can repeat at will.
//!
//! A log line the guest can provoke needs a cap, or the guest drives
//! the host log and evicts every other record from the async drain.
//! One bucket per log site keeps one noisy source from spending the
//! budget of another.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Admits up to `burst` events at once, then one more every `period`.
pub struct TokenBucket {
    burst: u32,
    period: Duration,
    state: Mutex<State>,
}

struct State {
    tokens: u32,
    refilled: Instant,
}

impl TokenBucket {
    pub fn new(burst: u32, period: Duration) -> Self {
        Self::starting_at(burst, period, Instant::now())
    }

    /// A full bucket whose refill counts from `start`.
    pub fn starting_at(burst: u32, period: Duration, start: Instant) -> Self {
        Self {
            burst,
            period,
            state: Mutex::new(State {
                tokens: burst,
                refilled: start,
            }),
        }
    }

    /// Take one token, or report that none is left.
    pub fn take(&self) -> bool {
        self.take_at(Instant::now())
    }

    /// [`Self::take`] at a given time.
    pub fn take_at(&self, now: Instant) -> bool {
        // The state is two integers a panic cannot leave half written.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.period.is_zero() {
            let elapsed = now.saturating_duration_since(state.refilled);
            let earned = elapsed.as_nanos() / self.period.as_nanos();
            if earned > 0 {
                let earned = u32::try_from(earned).unwrap_or(u32::MAX);
                state.tokens =
                    state.tokens.saturating_add(earned).min(self.burst);
                // Advance by whole periods so the remainder is kept.
                state.refilled += self.period * earned;
            }
        }
        if state.tokens == 0 {
            return false;
        }
        state.tokens -= 1;
        true
    }
}

/// What a paced change may do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// Make the change.
    Change,
    /// The budget is spent. Take down what the guest moved away from,
    /// and put nothing new up.
    Clear,
    /// Change nothing until the budget refills.
    Wait,
}

/// A budget for a change the guest can ask for at will and that costs
/// the host more than the guest.
///
/// Past the budget the guest gets one clear, so nothing is left on an
/// address it gave up, and then nothing until a token comes back. The
/// caller tries again on the guest's next access.
pub struct Pacer {
    burst: u32,
    period: Duration,
    /// Made on the first change, so the refill counts from it.
    bucket: Option<TokenBucket>,
    clear_spent: bool,
    throttled: bool,
    reported: bool,
}

impl Pacer {
    pub fn new(burst: u32, period: Duration) -> Self {
        Self {
            burst,
            period,
            bucket: None,
            clear_spent: false,
            throttled: false,
            reported: false,
        }
    }

    /// What may change at `now`. `cleared` says nothing is up already.
    pub fn admit(&mut self, now: Instant, cleared: bool) -> Pace {
        let (burst, period) = (self.burst, self.period);
        let bucket = self.bucket.get_or_insert_with(|| {
            TokenBucket::starting_at(burst, period, now)
        });
        if bucket.take_at(now) {
            self.clear_spent = false;
            return Pace::Change;
        }
        if cleared || self.clear_spent {
            return Pace::Wait;
        }
        self.clear_spent = true;
        self.throttled = true;
        Pace::Clear
    }

    /// Whether the last answer was the first clear ever, so the caller
    /// logs once per device and not at the guest's rate.
    pub fn first_throttle(&mut self) -> bool {
        std::mem::take(&mut self.throttled)
            && !std::mem::replace(&mut self.reported, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pacer_past_its_budget_clears_once_and_then_waits() {
        let mut pacer = Pacer::new(2, Duration::from_secs(1));
        let now = Instant::now();

        assert_eq!(pacer.admit(now, false), Pace::Change);
        assert_eq!(pacer.admit(now, false), Pace::Change);
        assert_eq!(pacer.admit(now, false), Pace::Clear);
        assert!(pacer.first_throttle());
        assert_eq!(pacer.admit(now, false), Pace::Wait);
        assert_eq!(pacer.admit(now, true), Pace::Wait);
        assert!(!pacer.first_throttle());

        let later = now + Duration::from_secs(2);
        assert_eq!(pacer.admit(later, true), Pace::Change);
    }

    #[test]
    fn nothing_up_needs_no_clear() {
        let mut pacer = Pacer::new(0, Duration::from_secs(1));
        assert_eq!(pacer.admit(Instant::now(), true), Pace::Wait);
    }

    #[test]
    fn a_burst_is_admitted_and_then_refused() {
        let bucket = TokenBucket::new(3, Duration::from_secs(60));
        let now = Instant::now();

        assert!(bucket.take_at(now));
        assert!(bucket.take_at(now));
        assert!(bucket.take_at(now));
        assert!(!bucket.take_at(now));
    }

    #[test]
    fn one_token_returns_per_period() {
        let period = Duration::from_secs(10);
        let bucket = TokenBucket::new(1, period);
        let start = Instant::now();

        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + period / 2));
        assert!(bucket.take_at(start + period));
        assert!(!bucket.take_at(start + period));
        // Two periods earn two tokens, but the burst caps them at one.
        assert!(bucket.take_at(start + period * 4));
        assert!(!bucket.take_at(start + period * 4));
    }

    #[test]
    fn the_remainder_of_a_period_is_kept() {
        let period = Duration::from_secs(10);
        let bucket = TokenBucket::new(1, period);
        let start = Instant::now();

        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + Duration::from_secs(7)));
        // 7 s + 3 s is one whole period from the start, not from the
        // last refused take.
        assert!(bucket.take_at(start + Duration::from_secs(10)));
    }

    #[test]
    fn a_zero_period_never_refills() {
        let bucket = TokenBucket::new(1, Duration::ZERO);
        let start = Instant::now();

        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + Duration::from_secs(3600)));
    }
}
