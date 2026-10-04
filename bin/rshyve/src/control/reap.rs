// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Retrying a pause release that nobody else will retry.

use std::thread;
use std::time::Duration;

use slog::{error, Logger};

#[derive(Debug, Clone, Copy)]
pub(super) struct Cadence {
    pub(super) fast_tries: u32,
    pub(super) fast: Duration,
    pub(super) slow: Duration,
}

/// A held pause keeps the guest's whole memory on this node, so the
/// retries never stop. The first retry runs at once, the next four 2
/// seconds apart, and the rest 30 seconds apart, so a release that
/// fails for good does not fill the log.
pub(super) const CADENCE: Cadence = Cadence {
    fast_tries: 5,
    fast: Duration::from_secs(2),
    slow: Duration::from_secs(30),
};

/// Retry `release` while `must_reap`. `stuck` is told when the fast
/// tries are spent, so a status shows a VM that cannot finish its stop,
/// and told again when the retries end.
pub(super) fn reap<E: std::fmt::Display>(
    cadence: Cadence,
    must_reap: impl Fn() -> bool,
    release: impl Fn() -> Result<(), E>,
    stuck: impl Fn(bool),
    log: &Logger,
) {
    let mut attempt: u64 = 0;
    while must_reap() {
        attempt += 1;
        match release() {
            Ok(()) => break,
            Err(e) => error!(log, "failed to release a held pause";
                "attempt" => attempt, "error" => %e),
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
        thread::sleep(wait);
    }
    stuck(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: Cadence = Cadence {
        fast_tries: 2,
        fast: Duration::ZERO,
        slow: Duration::ZERO,
    };

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
            &discard(),
        );
        assert_eq!(*reports.lock().expect("reports"), [true, false]);
    }
}
