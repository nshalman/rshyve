// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the backend generation barrier.
//!
//! A write already inside its syscall when a reset lands still reaches
//! the disk. It cannot touch guest memory, but it must not overtake a
//! write the next driver sends to the same sector.

use std::sync::atomic::AtomicBool;

use super::*;

/// Long enough that a barrier which does not hold new work back would
/// have let it through.
const LONG_ENOUGH_TO_OVERTAKE: Duration = Duration::from_millis(200);

/// A wait past this is a barrier holding work back, not a slow thread.
const HELD_BACK: Duration = Duration::from_secs(5);

// The decisive ordering case: a write admitted before the reset must
// land before any backend work of the next session.
#[test]
fn a_stale_write_holds_new_generation_io_behind_it() {
    let barrier = Arc::new(BackendBarrier::new());
    let stale = barrier.admit(0, true).expect("the first write is admitted");
    barrier.retire(1);

    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let fresh = {
        let barrier = Arc::clone(&barrier);
        let order = Arc::clone(&order);
        thread::spawn(move || {
            let _ticket =
                barrier.admit(1, true).expect("the new session is admitted");
            order.lock().expect("order lock").push("new");
        })
    };

    thread::sleep(LONG_ENOUGH_TO_OVERTAKE);
    assert!(
        order.lock().expect("order lock").is_empty(),
        "new work reached the disk before the stale write finished"
    );

    order.lock().expect("order lock").push("old");
    drop(stale);
    fresh.join().expect("the new write never ran");

    assert_eq!(
        *order.lock().expect("order lock"),
        ["old", "new"],
        "the stale write did not land first"
    );
}

// Work the reset cancelled is refused rather than run.
#[test]
fn a_cancelled_generation_is_refused() {
    let barrier = BackendBarrier::new();
    barrier.retire(1);
    assert!(
        barrier.admit(0, true).is_none(),
        "a request from the closed session reached the disk"
    );
    assert!(barrier.admit(1, true).is_some());
}

// A read cannot put the wrong bytes in a sector, so one that never
// returns must not wedge the next session's I/O.
#[test]
fn a_read_does_not_hold_the_next_generation_back() {
    let barrier = Arc::new(BackendBarrier::new());
    let stuck = barrier.admit(0, false).expect("the read is admitted");
    barrier.retire(1);

    // On another thread with a deadline. A barrier that counts the read
    // as a blocker makes this wait for a ticket the test still holds,
    // and that is a deadlock, not a slow answer: it has to be reported
    // as a failure rather than parked on.
    let admitted = Arc::new(AtomicBool::new(false));
    let probe = {
        let barrier = Arc::clone(&barrier);
        let admitted = Arc::clone(&admitted);
        thread::spawn(move || {
            let ticket = barrier.admit(1, true);
            admitted.store(ticket.is_some(), Ordering::Release);
        })
    };

    let deadline = Instant::now() + HELD_BACK;
    while !probe.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a read held the next session's writes back"
        );
        thread::sleep(Duration::from_millis(1));
    }
    probe.join().expect("the probe finished");
    assert!(
        admitted.load(Ordering::Acquire),
        "a read held the next session's writes back"
    );
    drop(stuck);
}

// A reset must wake the waiters it has just cancelled. Their own
// generation is over, so they have to give up now. Waiting for the
// stale write to end is not good enough: it may never end, and the
// worker would then sit on a session the guest has already replaced.
#[test]
fn a_retire_wakes_the_waiters_it_cancelled() {
    let barrier = Arc::new(BackendBarrier::new());
    // Held for the whole test, so nothing but `retire` can wake anyone.
    let never_ends = barrier.admit(0, true).expect("the first write is in");
    barrier.retire(1);

    let refused = Arc::new(AtomicBool::new(false));
    let waiter = {
        let barrier = Arc::clone(&barrier);
        let refused = Arc::clone(&refused);
        thread::spawn(move || {
            let admitted = barrier.admit(1, true).is_some();
            refused.store(!admitted, Ordering::Release);
        })
    };
    thread::sleep(LONG_ENOUGH_TO_OVERTAKE);
    assert!(!waiter.is_finished(), "the waiter was never held back");

    // The only wake-up in the test. The stale write is still running.
    barrier.retire(2);

    let deadline = Instant::now() + HELD_BACK;
    while !waiter.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a retire left a cancelled waiter asleep"
        );
        thread::sleep(Duration::from_millis(1));
    }
    waiter.join().expect("the waiter woke");
    assert!(
        refused.load(Ordering::Acquire),
        "work from an ended session was admitted"
    );
    drop(never_ends);
}

// A waiter must give up when its own session ends while it waits.
#[test]
fn a_waiter_gives_up_when_its_session_ends() {
    let barrier = Arc::new(BackendBarrier::new());
    let stale = barrier.admit(0, true).expect("the first write is admitted");
    barrier.retire(1);

    let refused = Arc::new(AtomicBool::new(false));
    let waiter = {
        let barrier = Arc::clone(&barrier);
        let refused = Arc::clone(&refused);
        thread::spawn(move || {
            let admitted = barrier.admit(1, true).is_some();
            refused.store(!admitted, Ordering::Release);
        })
    };

    thread::sleep(LONG_ENOUGH_TO_OVERTAKE);
    // A second reset while the waiter is still held back.
    barrier.retire(2);
    drop(stale);
    waiter.join().expect("the waiter never woke");

    assert!(
        refused.load(Ordering::Acquire),
        "work from an ended session was admitted after it woke"
    );
}
