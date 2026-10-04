// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Waiting on work a stop must not be held behind.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::codec::MigrateError;

/// How often a wait looks for a stop.
pub const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Set once the waiter gave up, so work left behind can end early.
#[derive(Clone, Debug, Default)]
pub struct Abandoned(Arc<AtomicBool>);

impl Abandoned {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Run `work` on its own thread, and wait for it until `cancelled`.
///
/// Work left behind runs to its end and its result is dropped, so it
/// must hold nothing the stop needs back.
pub fn unless_cancelled<T: Send + 'static>(
    name: &str,
    cancelled: impl Fn() -> bool,
    work: impl FnOnce(&Abandoned) -> T + Send + 'static,
) -> Result<T, MigrateError> {
    let abandoned = Abandoned::default();
    let for_work = abandoned.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // The waiter may have gone; the result then has no reader.
            drop(tx.send(work(&for_work)));
        })
        .map_err(|e| MigrateError::Io(format!("{name}: {e}")))?;
    loop {
        if cancelled() {
            abandoned.0.store(true, Ordering::Release);
            return Err(crate::cancelled());
        }
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(done) => return Ok(done),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(MigrateError::Io(format!(
                    "{name}: the worker ended without a result"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_stop_frees_a_wait_on_work_that_never_ends() {
        let (_keep, never) = mpsc::channel::<()>();
        let (left_tx, left) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let by_stop = Arc::clone(&stop);
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            by_stop.store(true, Ordering::Release);
        });

        let started = Instant::now();
        let error = unless_cancelled(
            "test-hang",
            || stop.load(Ordering::Acquire),
            move |abandoned| {
                let _ = never.recv_timeout(Duration::from_secs(1));
                let _ = left_tx.send(abandoned.get());
            },
        )
        .expect_err("cancelled");

        assert!(crate::is_cancel(&error), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
        stopper.join().expect("stopper");
        assert!(
            left.recv_timeout(Duration::from_secs(10))
                .expect("work ended"),
            "the work was not told the wait gave up"
        );
    }

    #[test]
    fn finished_work_hands_back_its_result() {
        assert_eq!(
            unless_cancelled("test-done", || false, |_| 7).ok(),
            Some(7)
        );
    }

    #[test]
    fn work_that_dies_is_an_error_not_a_hang() {
        let error = unless_cancelled(
            "test-panic",
            || false,
            |_| -> () { panic!("worker") },
        )
        .expect_err("died");
        assert!(error.to_string().contains("without a result"), "{error}");
    }
}
