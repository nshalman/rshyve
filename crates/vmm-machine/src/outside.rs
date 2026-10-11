// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device work that runs on a thread teardown does not own.

use std::fmt;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use vmm_devices::{FlushError, FlushIntent};

use crate::registry::DeviceRegistry;

/// Counts device work that teardown must wait out.
///
/// A migration's durable flush runs on a helper a stop can leave
/// behind. Teardown must not quiesce, flush, halt or destroy under it,
/// so it closes this first and waits for the helper to leave.
#[derive(Debug, Default)]
pub struct OutsideWork {
    state: Mutex<Outside>,
    left: Condvar,
}

#[derive(Debug, Default)]
struct Outside {
    running: usize,
    closed: bool,
}

/// Held while outside work touches devices.
#[must_use]
pub struct OutsideGuard<'a>(&'a OutsideWork);

/// How the wait for outside work ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// Nothing was running.
    Idle,
    /// Something was running and has left.
    Waited,
    /// Something is still running.
    TimedOut,
}

impl OutsideWork {
    /// Start outside work, or `None` once teardown has begun.
    pub fn enter(&self) -> Option<OutsideGuard<'_>> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        state.running += 1;
        Some(OutsideGuard(self))
    }

    /// Refuse new work, then wait up to `budget` for the running work.
    pub fn close(&self, budget: Duration) -> Drain {
        let deadline = Instant::now().checked_add(budget);
        let mut state = self.lock();
        state.closed = true;
        if state.running == 0 {
            return Drain::Idle;
        }
        while state.running > 0 {
            let left = match deadline {
                Some(deadline) => {
                    deadline.saturating_duration_since(Instant::now())
                }
                None => budget,
            };
            if left.is_zero() {
                return Drain::TimedOut;
            }
            state = self
                .left
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Drain::Waited
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Outside> {
        // A panic in outside work must not stop teardown from closing.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for OutsideGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.running -= 1;
        if state.running == 0 {
            self.0.left.notify_all();
        }
    }
}

/// Why a durable flush outside teardown did not finish.
#[derive(Debug)]
pub enum OutsideFlushError {
    /// Teardown began first, so nothing was flushed.
    TearingDown,
    /// The caller gave up, so the devices left were not flushed.
    Abandoned,
    /// A device's flush failed, so the devices after it were not
    /// flushed.
    Device {
        device: &'static str,
        source: FlushError,
    },
}

impl fmt::Display for OutsideFlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TearingDown => f.write_str("the VM is being torn down"),
            Self::Abandoned => f.write_str("the flush was abandoned"),
            Self::Device { device, source } => write!(f, "{device}: {source}"),
        }
    }
}

impl std::error::Error for OutsideFlushError {}

/// Make every backing store durable from a thread teardown does not
/// own, and stop between devices once `abandoned` says so.
pub fn flush_durable(
    registry: &DeviceRegistry,
    abandoned: impl Fn() -> bool,
) -> Result<(), OutsideFlushError> {
    let Some(_running) = registry.outside_work().enter() else {
        return Err(OutsideFlushError::TearingDown);
    };
    for device in registry.lifecycle_devices() {
        if abandoned() {
            return Err(OutsideFlushError::Abandoned);
        }
        device
            .flush_backing(FlushIntent::Durable)
            .map_err(|source| OutsideFlushError::Device {
                device: device.type_name(),
                source,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn nothing_starts_once_teardown_closed() {
        let work = OutsideWork::default();
        assert_eq!(work.close(Duration::ZERO), Drain::Idle);
        assert!(work.enter().is_none());
    }

    #[test]
    fn teardown_waits_for_running_work_to_leave() {
        let work = OutsideWork::default();
        let (tx, rx) = mpsc::channel::<()>();
        thread::scope(|s| {
            let guard = work.enter().expect("open");
            s.spawn(move || {
                let _guard = guard;
                let _ = rx.recv_timeout(Duration::from_millis(100));
            });
            assert_eq!(work.close(Duration::from_secs(10)), Drain::Waited);
            drop(tx);
        });
    }

    #[test]
    fn a_wait_on_work_that_never_leaves_ends() {
        let work = OutsideWork::default();
        let _held = work.enter().expect("open");
        assert_eq!(work.close(Duration::from_millis(50)), Drain::TimedOut);
    }
}
