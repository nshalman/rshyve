// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Mutex;

use super::*;

/// One test owns the whole flag, because a second test that wrote it
/// would race this one.
#[test]
fn handler_write_is_visible_through_both_accessors() {
    // A static cannot be split across a crate boundary, and the
    // handler cannot capture, so the accessors must hand back the
    // same cell the handler writes.
    assert!(std::ptr::eq(sigterm_flag(), &SIGTERM_RECEIVED));
    assert!(!sigterm_received());

    sigterm_handler(libc::SIGTERM);

    assert!(sigterm_received());
    assert!(sigterm_flag().load(Ordering::Relaxed));
    SIGTERM_RECEIVED.store(false, Ordering::Relaxed);
}

/// Records every step a shutdown takes, in order.
struct RecordingStop {
    steps: Mutex<Vec<String>>,
    poweroff_refused: bool,
    halt_refused: bool,
}

impl RecordingStop {
    fn new() -> Self {
        Self {
            steps: Mutex::new(Vec::new()),
            poweroff_refused: false,
            halt_refused: false,
        }
    }

    fn push(&self, step: &str) {
        self.steps
            .lock()
            .expect("steps lock")
            .push(step.to_string());
    }

    fn steps(&self) -> Vec<String> {
        self.steps.lock().expect("steps lock").clone()
    }
}

fn refused() -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::PermissionDenied))
}

impl GuestStop for RecordingStop {
    fn request_poweroff(&self) -> io::Result<()> {
        self.push("poweroff");
        if self.poweroff_refused {
            return refused();
        }
        Ok(())
    }

    fn halt(&self) -> io::Result<()> {
        self.push("halt");
        if self.halt_refused {
            return refused();
        }
        Ok(())
    }
}

fn null_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

/// Drive one sequence with no real waiting, and say whether it ended
/// the process.
fn drive(stop: &RecordingStop) -> bool {
    let gave_up = Mutex::new(false);
    shutdown_sequence(
        stop,
        |budget| stop.push(&format!("wait {}s", budget.as_secs())),
        || *gave_up.lock().expect("gave up lock") = true,
        &null_log(),
    );
    let ended = *gave_up.lock().expect("gave up lock");
    ended
}

#[test]
fn a_stop_asks_before_it_forces() {
    // A guest that acts on the request flushes its filesystems, so the
    // hard stop must come second and only after a wait.
    let stop = RecordingStop::new();

    let ended = drive(&stop);

    assert_eq!(
        stop.steps(),
        [
            "poweroff".to_string(),
            format!("wait {}s", GRACEFUL_BUDGET.as_secs()),
            "halt".to_string(),
            format!("wait {}s", HARD_BUDGET.as_secs()),
        ]
    );
    assert!(ended, "a VMM that outlived its own shutdown must end");
}

#[test]
fn a_refused_request_still_reaches_the_halt() {
    let mut stop = RecordingStop::new();
    stop.poweroff_refused = true;

    let ended = drive(&stop);

    assert!(stop.steps().contains(&"halt".to_string()));
    assert!(ended);
}

#[test]
fn a_refused_halt_still_ends_the_process() {
    // Nothing is left to try, and a node must not keep a VM that will
    // not stop.
    let mut stop = RecordingStop::new();
    stop.halt_refused = true;

    let ended = drive(&stop);

    assert!(ended);
}

#[test]
fn the_hard_budget_outlasts_the_teardown_it_waits_on() {
    // Teardown gets 5 s to quiesce the devices and 5 s to join the
    // vCPU threads. A shorter wait would kill a teardown that works.
    assert!(HARD_BUDGET > Duration::from_secs(10));
}
