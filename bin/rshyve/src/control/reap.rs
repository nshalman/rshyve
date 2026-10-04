// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Retrying a pause release that nobody else will retry.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
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

type GiveUp = Box<dyn FnOnce() + Send>;

enum Arm {
    Armed(GiveUp),
    Disarmed,
    Fired,
}

type Shared = (Mutex<Arm>, Condvar);

/// Runs `give_up` once its clock runs out, from a thread of its own,
/// unless dropped first.
///
/// WHY a thread: a release can hang in its ioctl, and a check made by
/// the reaper between releases would then never run.
struct Deadline {
    shared: Arc<Shared>,
    at: Option<Instant>,
}

impl Deadline {
    fn new(give_up: GiveUp) -> Self {
        Self {
            shared: Arc::new((Mutex::new(Arm::Armed(give_up)), Condvar::new())),
            at: None,
        }
    }

    /// Start the clock on the first call; return the time left.
    fn start(&mut self, after: Duration, log: &Logger) -> Duration {
        let at = *self.at.get_or_insert_with(|| {
            let at = Instant::now() + after;
            let shared = Arc::clone(&self.shared);
            let spawned = thread::Builder::new()
                .name("release-deadline".into())
                .spawn(move || watch(&shared, at));
            if let Err(e) = spawned {
                // The reaper still fires it between releases.
                error!(log, "no timer for the release deadline";
                    "error" => %e);
            }
            at
        });
        at.saturating_duration_since(Instant::now())
    }

    fn fire(&self) {
        fire(&self.shared);
    }

    fn fired(&self) -> bool {
        matches!(*lock(&self.shared), Arm::Fired)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        let mut arm = lock(&self.shared);
        if matches!(*arm, Arm::Armed(_)) {
            *arm = Arm::Disarmed;
        }
        self.shared.1.notify_all();
    }
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Arm> {
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

fn watch(shared: &Shared, at: Instant) {
    let mut arm = lock(shared);
    while matches!(*arm, Arm::Armed(_)) {
        let left = at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            drop(arm);
            fire(shared);
            return;
        }
        arm = shared
            .1
            .wait_timeout(arm, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// Run `give_up` unless it ran or was disarmed.
fn fire(shared: &Shared) {
    let mut arm = lock(shared);
    if !matches!(*arm, Arm::Armed(_)) {
        return;
    }
    if let Arm::Armed(give_up) = std::mem::replace(&mut *arm, Arm::Fired) {
        drop(arm);
        give_up();
    }
}

/// Retry `release` while `must_reap`. `stuck` is told when the fast
/// tries are spent, and again when the retries end. `give_up` runs once
/// `give_up_after` has passed since the first failed release, even when
/// a release is still hung; in production it does not return.
pub(super) fn reap<E: std::fmt::Display>(
    cadence: Cadence,
    must_reap: impl Fn() -> bool,
    release: impl Fn() -> Result<(), E>,
    stuck: impl Fn(bool),
    give_up: impl FnOnce() + Send + 'static,
    log: &Logger,
) {
    let mut deadline = {
        let log = log.clone();
        let secs = cadence.give_up_after.as_secs();
        Deadline::new(Box::new(move || {
            error!(log, "a held pause would not release; ending the \
                process so the node gets the guest's memory back";
                "failing_secs" => secs);
            give_up();
        }))
    };
    let mut attempt: u64 = 0;
    loop {
        if deadline.fired() {
            // The status keeps saying why until the process ends.
            return;
        }
        if !must_reap() {
            break;
        }
        attempt += 1;
        match release() {
            Ok(()) => break,
            Err(e) => error!(log, "failed to release a held pause";
                "attempt" => attempt, "error" => %e),
        }
        let left = deadline.start(cadence.give_up_after, log);
        if left.is_zero() {
            if !must_reap() {
                break;
            }
            deadline.fire();
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
        sleep_while(wait.min(left), &|| !deadline.fired() && must_reap());
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

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
        let reports = Mutex::new(Vec::new());
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
        let reports = Mutex::new(Vec::new());
        let gave_up = Arc::new(AtomicBool::new(false));
        let calls = AtomicUsize::new(0);
        let flag = Arc::clone(&gave_up);
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
            move || flag.store(true, Ordering::Release),
            &discard(),
        );
        assert!(gave_up.load(Ordering::Acquire));
        assert!(calls.load(Ordering::Acquire) > 2);
        // The status keeps saying why until the process ends.
        assert_eq!(*reports.lock().expect("reports"), [true]);
    }

    #[test]
    fn a_hung_release_still_ends_the_process() {
        let (gave_up_tx, gave_up) = mpsc::channel();
        let (unblock, blocked) = mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let reports = Mutex::new(Vec::new());
        let calls = AtomicUsize::new(0);
        thread::scope(|s| {
            let reaper = s.spawn(|| {
                reap(
                    Cadence {
                        give_up_after: Duration::from_millis(50),
                        ..NOW
                    },
                    || true,
                    || {
                        if calls.fetch_add(1, Ordering::AcqRel) > 0 {
                            let _ = blocked.lock().expect("blocked").recv();
                        }
                        Err("EBUSY")
                    },
                    |stuck| reports.lock().expect("reports").push(stuck),
                    move || gave_up_tx.send(()).expect("gave up"),
                    &discard(),
                );
            });
            gave_up
                .recv_timeout(Duration::from_secs(10))
                .expect("the deadline fired under a hung release");
            assert!(!reaper.is_finished(), "the release returned");
            drop(unblock);
        });
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert!(!reports.lock().expect("reports").contains(&false));
    }

    #[test]
    fn a_release_that_works_disarms_the_deadline() {
        let gave_up = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&gave_up);
        let calls = AtomicUsize::new(0);
        reap(
            Cadence {
                give_up_after: Duration::from_millis(50),
                ..NOW
            },
            || true,
            || match calls.fetch_add(1, Ordering::AcqRel) {
                0 => Err("EBUSY"),
                _ => Ok(()),
            },
            |_| {},
            move || flag.store(true, Ordering::Release),
            &discard(),
        );
        thread::sleep(Duration::from_millis(200));
        assert!(!gave_up.load(Ordering::Acquire));
    }

    #[test]
    fn a_release_made_elsewhere_ends_the_wait_at_once() {
        let held = AtomicBool::new(true);
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
