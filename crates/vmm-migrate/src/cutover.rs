// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One deadline for the whole cutover, from the gate's go to the
//! commit.
//!
//! Each read has its own deadline and each send a stall limit. A peer
//! that sends or reads one byte at a time meets both, and so holds the
//! paused guest without end. When this deadline passes first, the
//! cutover fails and the source rolls back: the destination never
//! starts a guest without the commit, so the source can safely run it
//! again.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::codec::MigrateError;
use crate::wire::Shutter;
use crate::{GateState, MigrationStatus, StopGate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not armed yet. Nothing can fire.
    Idle,
    Armed(Instant),
    /// The deadline passed before the commit. The commit is refused.
    Fired,
    /// The commit was claimed, or the migration ended. Nothing fires.
    Done,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// The cutover deadline of one source migration.
///
/// The thread starts with the migration, so arming it after the go
/// cannot fail. Dropping this stops the thread.
pub struct Cutover {
    shared: Arc<Shared>,
    status: Arc<Mutex<MigrationStatus>>,
    budget: Duration,
}

impl Cutover {
    /// Start the watcher. On expiry it records the gate as aborted,
    /// then runs `shut` to free the I/O the migration is blocked on.
    pub fn start(
        budget: Duration,
        shut: Shutter,
        status: Arc<Mutex<MigrationStatus>>,
        log: &slog::Logger,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::Idle),
            wake: Condvar::new(),
        });
        let watched = Arc::clone(&shared);
        let for_watch = Arc::clone(&status);
        let log = log.clone();
        std::thread::Builder::new()
            .name("migrate-cutover".into())
            .spawn(move || {
                if watch(&watched, &for_watch) {
                    slog::error!(log, "no commit within the cutover \
                        deadline; the source rolls back";
                        "budget_s" => budget.as_secs());
                    shut();
                }
            })?;
        Ok(Self {
            shared,
            status,
            budget,
        })
    }

    /// Start the clock. Called once the gate said go, or where the
    /// gate would be when there is none.
    pub fn arm(&self) {
        let mut state = lock(&self.shared);
        if *state == State::Idle {
            *state = State::Armed(Instant::now() + self.budget);
            self.shared.wake.notify_all();
        }
    }

    /// Claim the commit. Refused once the deadline passed, whether or
    /// not the watcher has woken for it yet.
    pub fn claim(&self) -> Result<(), MigrateError> {
        let mut state = lock(&self.shared);
        if fire_if_due(&mut state, &self.status) {
            return Err(self.error());
        }
        *state = State::Done;
        self.shared.wake.notify_all();
        Ok(())
    }

    /// End the watch. Whether the deadline fired; once this returns,
    /// it never will.
    pub fn finish(&self) -> bool {
        let mut state = lock(&self.shared);
        let fired = fire_if_due(&mut state, &self.status);
        *state = if fired { State::Fired } else { State::Done };
        self.shared.wake.notify_all();
        fired
    }

    /// The error of a cutover that ran out of time, in place of the
    /// broken stream the shutdown caused.
    pub fn expired_or(&self, error: MigrateError) -> MigrateError {
        if *lock(&self.shared) == State::Fired {
            return self.error();
        }
        error
    }

    fn error(&self) -> MigrateError {
        MigrateError::Phase(format!(
            "no commit within {} s of the cutover; the source resumes",
            self.budget.as_secs()
        ))
    }
}

impl Drop for Cutover {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A gate whose commit first claims the cutover deadline.
pub struct Gated<'a> {
    pub inner: &'a dyn StopGate,
    pub cutover: &'a Cutover,
}

impl StopGate for Gated<'_> {
    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }

    fn commit(&self) -> Result<(), MigrateError> {
        self.cutover.claim()?;
        self.inner.commit()
    }

    fn on_stop(&self, shut: Box<dyn FnOnce() + Send>) {
        self.inner.on_stop(shut)
    }
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Fire when the deadline has passed. True when it has fired, now or
/// before.
///
/// Under the state lock, so a claim or a finish sees either no fire or
/// a fire with the gate already marked: the owner then reads `abort`
/// and rolls back.
fn fire_if_due(state: &mut State, status: &Mutex<MigrationStatus>) -> bool {
    match *state {
        State::Fired => true,
        State::Armed(until) if Instant::now() >= until => {
            *state = State::Fired;
            if let Ok(mut s) = status.lock() {
                if s.gate == GateState::Go {
                    s.gate = GateState::Abort;
                }
            }
            true
        }
        _ => false,
    }
}

/// Wait for the deadline. True when it fired.
fn watch(shared: &Shared, status: &Mutex<MigrationStatus>) -> bool {
    let mut state = lock(shared);
    loop {
        match *state {
            State::Idle => {
                state =
                    shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            State::Armed(until) => {
                if fire_if_due(&mut state, status) {
                    return true;
                }
                let left = until.saturating_duration_since(Instant::now());
                state = shared
                    .wake
                    .wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            State::Fired | State::Done => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn discard() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn started(
        budget: Duration,
    ) -> (Cutover, Arc<AtomicBool>, Arc<Mutex<MigrationStatus>>) {
        let shut = Arc::new(AtomicBool::new(false));
        let by_shut = Arc::clone(&shut);
        let status = Arc::new(Mutex::new(MigrationStatus {
            gate: GateState::Go,
            ..MigrationStatus::default()
        }));
        let cutover = Cutover::start(
            budget,
            Box::new(move || by_shut.store(true, Ordering::Release)),
            Arc::clone(&status),
            &discard(),
        )
        .expect("start");
        (cutover, shut, status)
    }

    fn wait_for(flag: &AtomicBool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !flag.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn an_unarmed_cutover_never_fires() {
        let (cutover, shut, _) = started(Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!shut.load(Ordering::Acquire));
        cutover.claim().expect("claim");
    }

    #[test]
    fn an_expired_cutover_refuses_the_commit_and_aborts_the_gate() {
        let (cutover, shut, status) = started(Duration::from_millis(50));
        cutover.arm();
        wait_for(&shut);
        assert!(shut.load(Ordering::Acquire), "the stream was not shut");
        assert_eq!(status.lock().expect("status").gate, GateState::Abort);
        assert!(cutover.claim().is_err());
        assert!(cutover.finish());
    }

    #[test]
    fn a_late_claim_is_refused_even_before_the_watcher_wakes() {
        // The watcher thread may run late; the deadline does not.
        let (cutover, _, status) = started(Duration::from_millis(1));
        cutover.arm();
        std::thread::sleep(Duration::from_millis(20));
        let refused = cutover.claim().is_err();
        assert!(refused, "a commit after the deadline was taken");
        assert_eq!(status.lock().expect("status").gate, GateState::Abort);
    }

    #[test]
    fn a_claimed_commit_never_fires() {
        let (cutover, shut, status) = started(Duration::from_millis(50));
        cutover.arm();
        cutover.claim().expect("in time");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!shut.load(Ordering::Acquire));
        assert_eq!(status.lock().expect("status").gate, GateState::Go);
        assert!(!cutover.finish());
    }
}
