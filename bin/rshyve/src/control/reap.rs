// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Retrying a pause release that nobody else will retry.

use std::thread;
use std::time::{Duration, Instant};

use slog::{error, Logger};

#[derive(Debug, Clone, Copy)]
pub(super) struct Cadence {
    pub(super) fast_tries: u32,
    pub(super) fast: Duration,
    pub(super) slow: Duration,
    /// How long releases may keep failing before the process ends.
    pub(super) give_up_after: Duration,
}

/// Slow retries keep a release that fails for good out of the log.
pub(super) const CADENCE: Cadence = Cadence {
    fast_tries: 5,
    fast: Duration::from_secs(2),
    slow: Duration::from_secs(30),
    give_up_after: RELEASE_GIVE_UP,
};

/// A held pause keeps the guest's whole memory on this node. Past this
/// the process ends, and the zone's restart policy and the instance's
/// autodestruct reclaim it.
const RELEASE_GIVE_UP: Duration = Duration::from_secs(300);

/// How often a wait between retries looks for a release made elsewhere.
const REAP_POLL: Duration = Duration::from_millis(250);

/// Retry `release` while `must_reap`. `stuck` is told when the fast
/// tries are spent, and again when the retries end. `give_up` runs once
/// releases have failed for `give_up_after`; in production it does not
/// return.
pub(super) fn reap<E: std::fmt::Display>(
    cadence: Cadence,
    must_reap: impl Fn() -> bool,
    release: impl Fn() -> Result<(), E>,
    stuck: impl Fn(bool),
    give_up: impl FnOnce(),
    log: &Logger,
) {
    let mut attempt: u64 = 0;
    let mut failing_since = None;
    while must_reap() {
        attempt += 1;
        match release() {
            Ok(()) => break,
            Err(e) => error!(log, "failed to release a held pause";
                "attempt" => attempt, "error" => %e),
        }
        let failing = *failing_since.get_or_insert_with(Instant::now);
        let left = cadence.give_up_after.saturating_sub(failing.elapsed());
        if left.is_zero() {
            if !must_reap() {
                break;
            }
            error!(log, "a held pause would not release; ending the \
                process so the node gets the guest's memory back";
                "attempts" => attempt,
                "failing_secs" => failing.elapsed().as_secs());
            give_up();
            return;
        }
        let wait = if attempt < u64::from(cadence.fast_tries) {
            cadence.fast
        } else {
            if attempt == u64::from(cadence.fast_tries) {
                error!(log, "a held pause will not release; the VM cannot \
                    finish its stop, retrying slowly";
                    "retry_secs" => cadence.slow.as_secs());
                stuck(true);
            }
            cadence.slow
        };
        sleep_while(wait.min(left), &must_reap);
    }
    stuck(false);
}

/// Sleep for `total`, or less once `must_reap` turns false.
fn sleep_while(total: Duration, must_reap: &impl Fn() -> bool) {
    let mut left = total;
    while !left.is_zero() && must_reap() {
        let slice = left.min(REAP_POLL);
        thread::sleep(slice);
        left -= slice;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: Cadence = Cadence {
        fast_tries: 2,
        fast: Duration::ZERO,
        slow: Duration::ZERO,
        give_up_after: Duration::from_secs(3600),
    };

    fn never() {
        panic!("gave up");
    }

    fn discard() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn a_release_is_retried_past_the_fast_tries() {
        let calls = AtomicUsize::new(0);
        reap(
            NOW,
            || true,
            || match calls.fetch_add(1, Ordering::AcqRel) {
                0..=7 => Err("EBUSY"),
                _ => Ok(()),
            },
            |_| {},
            never,
            &discard(),
        );
        assert_eq!(calls.load(Ordering::Acquire), 9);
    }

    #[test]
    fn the_retries_end_once_nothing_is_left() {
        let calls = AtomicUsize::new(0);
        let checks = AtomicUsize::new(0);
        reap(
            NOW,
            || checks.fetch_add(1, Ordering::AcqRel) < 3,
            || {
                calls.fetch_add(1, Ordering::AcqRel);
                Err("EBUSY")
            },
            |_| {},
            never,
            &discard(),
        );
        assert_eq!(calls.load(Ordering::Acquire), 3);
    }

    #[test]
    fn nothing_is_released_when_nothing_is_held() {
        reap(
            NOW,
            || false,
            || -> Result<(), &str> { panic!("released") },
            |_| {},
            never,
            &discard(),
        );
    }

    #[test]
    fn a_release_that_keeps_failing_is_reported() {
        // Otherwise a stopped VM sits in `stopping` with nothing saying
        // why.
        let calls = AtomicUsize::new(0);
        let reports = std::sync::Mutex::new(Vec::new());
        reap(
            NOW,
            || true,
            || match calls.fetch_add(1, Ordering::AcqRel) {
                0..=3 => Err("EBUSY"),
                _ => Ok(()),
            },
            |stuck| reports.lock().expect("reports").push(stuck),
            never,
            &discard(),
        );
        assert_eq!(*reports.lock().expect("reports"), [true, false]);
    }

    #[test]
    fn a_release_that_fails_for_too_long_ends_the_process() {
        let reports = std::sync::Mutex::new(Vec::new());
        let gave_up = std::sync::atomic::AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        reap(
            Cadence {
                give_up_after: Duration::from_millis(50),
                ..NOW
            },
            || true,
            || {
                calls.fetch_add(1, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(5));
                Err("EBUSY")
            },
            |stuck| reports.lock().expect("reports").push(stuck),
            || gave_up.store(true, Ordering::Release),
            &discard(),
        );
        assert!(gave_up.load(Ordering::Acquire));
        assert!(calls.load(Ordering::Acquire) > 2);
        // The status keeps saying why until the process ends.
        assert_eq!(*reports.lock().expect("reports"), [true]);
    }

    #[test]
    fn a_release_made_elsewhere_ends_the_wait_at_once() {
        let held = std::sync::atomic::AtomicBool::new(true);
        let started = Instant::now();
        thread::scope(|s| {
            s.spawn(|| {
                thread::sleep(Duration::from_millis(100));
                held.store(false, Ordering::Release);
            });
            reap(
                Cadence {
                    fast_tries: 1,
                    slow: Duration::from_secs(30),
                    ..NOW
                },
                || held.load(Ordering::Acquire),
                || Err("EBUSY"),
                |_| {},
                never,
                &discard(),
            );
        });
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
