// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Retrying a pause release that nobody else will retry.

use std::thread;
use std::time::Duration;

use slog::{error, Logger};

/// The waits between release attempts.
#[derive(Debug, Clone, Copy)]
pub(super) struct Cadence {
    pub(super) fast_tries: u32,
    pub(super) fast: Duration,
    pub(super) slow: Duration,
}

/// A held pause keeps the guest's whole memory on this node, so the
/// retries never stop. The slow cadence keeps a release that fails for
/// good from filling the log.
pub(super) const CADENCE: Cadence = Cadence {
    fast_tries: 5,
    fast: Duration::from_secs(2),
    slow: Duration::from_secs(30),
};

pub(super) fn reap<E: std::fmt::Display>(
    cadence: Cadence,
    must_reap: impl Fn() -> bool,
    release: impl Fn() -> Result<(), E>,
    log: &Logger,
) {
    let mut attempt: u64 = 0;
    while must_reap() {
        attempt += 1;
        match release() {
            Ok(()) => return,
            Err(e) => error!(log, "failed to release a held pause";
                "attempt" => attempt, "error" => %e),
        }
        let wait = if attempt < u64::from(cadence.fast_tries) {
            cadence.fast
        } else {
            cadence.slow
        };
        thread::sleep(wait);
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
            &discard(),
        );
    }
}
