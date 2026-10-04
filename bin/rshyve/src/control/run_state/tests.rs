// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Barrier};

use VmRunState::{Faulted, Migrating, Paused, Running, Stopped, Stopping};

fn import(run: &RunControl) -> Result<(), String> {
    run.admit_import(|| None)
}

fn ok() -> Result<(), ()> {
    Ok(())
}

fn step_ok() -> Result<(), StepFailed<()>> {
    Ok(())
}

/// Counts the calls a stop makes, so a test can say which ran.
#[derive(Default)]
struct Calls {
    order: Mutex<Vec<&'static str>>,
}

impl Calls {
    fn note(&self, what: &'static str) -> Result<(), &'static str> {
        self.order.lock().expect("calls").push(what);
        Ok(())
    }

    fn count(&self, what: &str) -> usize {
        self.order
            .lock()
            .expect("calls")
            .iter()
            .filter(|w| **w == what)
            .count()
    }

    /// Holds, latches and releases.
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.count("hold"),
            self.count("latch"),
            self.count("release"),
        )
    }
}

fn stop(
    run: &RunControl,
    calls: &Calls,
) -> Result<(), StopFailure<&'static str>> {
    run.stop(
        STOP_WAIT,
        || calls.note("hold"),
        || calls.note("latch"),
        || calls.note("release"),
    )
}

/// A source that got as far as its vCPU pause.
fn parked_source() -> RunControl {
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    run.park_migration(ok).expect("parked");
    run
}

/// Wait until a stop on another thread is blocked on `changing`.
fn wait_for_a_blocked_stop(run: &RunControl) {
    while run.contended.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }
}

#[test]
fn an_import_cannot_claim_a_vm_that_is_still_pausing() {
    // The state already says Paused while the devices are parking.
    let run = RunControl::new(Running);
    run.change(Running, Paused, |held| {
        assert_eq!(run.get(), Paused);
        assert_eq!(import(&run), Err(CHANGE_IN_PROGRESS.to_string()));
        *held = true;
        step_ok()
    })
    .expect("pause");

    import(&run).expect("fully paused");
    assert_eq!(run.get(), Migrating);
}

#[test]
fn an_import_cannot_claim_a_vm_that_is_resuming() {
    let run = RunControl::new(Paused);
    run.change(Paused, Running, |held| {
        assert_eq!(import(&run), Err(CHANGE_IN_PROGRESS.to_string()));
        *held = false;
        step_ok()
    })
    .expect("resume");

    assert!(import(&run).is_err());
    assert_eq!(run.get(), Running);
}

#[test]
fn a_resume_after_an_import_claim_leaves_it_migrating() {
    let run = RunControl::new(Paused);
    import(&run).expect("claim");

    let resumed =
        run.change(Paused, Running, |_| -> Result<(), StepFailed<()>> {
            panic!("a resume must not touch a migrating VM")
        });

    assert_eq!(resumed, Err(ChangeFailed::WrongState));
    assert_eq!(run.get(), Migrating);
}

#[test]
fn a_second_import_is_refused() {
    let run = RunControl::new(Paused);
    import(&run).expect("first");
    assert!(import(&run).is_err());
    assert_eq!(run.get(), Migrating);
}

#[test]
fn of_racing_imports_exactly_one_wins() {
    for _ in 0..200 {
        let run = Arc::new(RunControl::new(Paused));
        let start = Arc::new(Barrier::new(2));
        let racers: Vec<_> = (0..2)
            .map(|_| {
                let run = Arc::clone(&run);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    import(&run).is_ok()
                })
            })
            .collect();
        let wins = racers
            .into_iter()
            .map(|r| r.join().expect("racer"))
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }
}

#[test]
fn a_refused_import_leaves_the_vm_paused() {
    let run = RunControl::new(Paused);
    let refused = run.admit_import(|| Some("blocked".to_string()));
    assert_eq!(refused, Err("blocked".to_string()));
    assert_eq!(run.get(), Paused);
    assert!(!run.topology.ejects_held());
}

#[test]
fn ejects_wait_for_the_whole_import() {
    // The import restores into the device set it was admitted with.
    let run = RunControl::new(Paused);
    import(&run).expect("claim");
    assert!(run.topology.ejects_held());

    run.end_migration(ok).expect("ended");
    assert!(!run.topology.ejects_held());
}

#[test]
fn ejects_wait_for_a_migrated_source_to_hand_off() {
    let run = parked_source();
    assert!(run.topology.ejects_held());
    run.hand_off(ok).expect("handed off");
    assert!(!run.topology.ejects_held());
}

#[test]
fn a_source_refusal_runs_under_the_topology_lock_and_rolls_back() {
    let run = RunControl::new(Running);
    let refused = run.admit_source(|| {
        assert_eq!(run.get(), Migrating);
        assert!(run.topology.is_held(), "topology not held");
        Some("grew".to_string())
    });
    assert_eq!(refused, Err("grew".to_string()));
    assert_eq!(run.get(), Running);
    assert!(!run.topology.ejects_held());
}

#[test]
fn an_import_refusal_runs_under_the_topology_lock() {
    let run = RunControl::new(Paused);
    run.admit_import(|| {
        assert!(run.topology.is_held(), "topology not held");
        None
    })
    .expect("admitted");
}

#[test]
fn a_source_cannot_claim_a_vm_that_is_resuming() {
    let run = RunControl::new(Paused);
    run.change(Paused, Running, |held| {
        let claimed = run.admit_source(|| None);
        assert_eq!(claimed, Err(CHANGE_IN_PROGRESS.to_string()));
        *held = false;
        step_ok()
    })
    .expect("resume");
    run.admit_source(|| None).expect("running");
}

#[test]
fn a_stop_during_a_source_refusal_waits_for_it() {
    let run = RunControl::new(Running);
    let calls = Calls::default();
    std::thread::scope(|s| {
        let refused = run.admit_source(|| {
            s.spawn(|| stop(&run, &calls).expect("stop"));
            wait_for_a_blocked_stop(&run);
            assert_eq!(calls.counts(), (0, 0, 0), "a stop ran inside it");
            Some("grew".to_string())
        });
        assert!(refused.is_err());
    });
    // The claim rolled back first, so the stop found a running VM.
    assert_eq!(calls.counts(), (0, 1, 0));
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_failed_pause_rolls_back_before_a_stop_runs() {
    let run = RunControl::new(Running);
    let failed =
        run.change(Running, Paused, |_| Err(StepFailed::Undone("stuck")));
    assert_eq!(failed, Err(ChangeFailed::Step("stuck")));
    assert_eq!(run.get(), Running);

    let calls = Calls::default();
    std::thread::scope(|s| {
        let failed = run.change(Running, Paused, |_| {
            s.spawn(|| stop(&run, &calls).expect("stop"));
            wait_for_a_blocked_stop(&run);
            Err(StepFailed::Undone("stuck"))
        });
        assert_eq!(failed, Err(ChangeFailed::Step("stuck")));
    });
    assert_eq!(calls.counts(), (0, 1, 0));
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_stop_waits_for_a_pause_in_progress_and_releases_its_hold() {
    // Paused is published before the vCPUs park. A stop that ran then
    // would see no pause to release, and the pause that lands after it
    // would hold the vCPUs away from the suspend for ever.
    let run = RunControl::new(Running);
    let calls = Calls::default();
    std::thread::scope(|s| {
        run.change(Running, Paused, |held| {
            s.spawn(|| stop(&run, &calls).expect("stop"));
            wait_for_a_blocked_stop(&run);
            assert_eq!(calls.counts(), (0, 0, 0), "a stop ran inside a pause");
            *held = true;
            step_ok()
        })
        .expect("pause");
    });

    assert_eq!(calls.counts(), (0, 1, 1));
    assert_eq!(run.get(), Stopping);
    assert!(!run.held());
}

#[test]
fn a_stop_gives_up_on_a_change_that_outlasts_its_wait() {
    // The SIGTERM watchdog calls the stop, and its budgets must run.
    let run = RunControl::new(Running);
    run.change(Running, Paused, |held| {
        std::thread::scope(|s| {
            let late = s
                .spawn(|| run.stop(Duration::from_millis(20), ok, ok, ok))
                .join()
                .expect("stop thread");
            assert_eq!(late, Err(StopFailure::Busy));
        });
        *held = true;
        step_ok()
    })
    .expect("pause");
    assert_eq!(run.get(), Paused);
}

#[test]
fn a_pause_runs_under_the_topology_lock() {
    // A hot-add holds it from its Running check to its last step, so
    // the device set a pause parks is the whole set.
    let run = RunControl::new(Running);
    run.change(Running, Paused, |held| {
        assert!(run.topology.is_held(), "topology not held");
        *held = true;
        step_ok()
    })
    .expect("pause");
}

#[test]
fn a_stop_parks_a_running_source_before_it_latches() {
    // Its vCPUs would reach the suspend at once and teardown would run
    // under the migration thread.
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    let calls = Calls::default();

    stop(&run, &calls).expect("stop");

    assert_eq!(*calls.order.lock().expect("calls"), ["hold", "latch"]);
    assert!(run.held());
    assert!(run.cancelled());
    let refused = run.park_migration(|| -> Result<(), ()> {
        panic!("paused a stopping VM")
    });
    assert_eq!(refused, Err(None));

    let end = Calls::default();
    assert_eq!(run.end_migration(|| end.note("release")), Ok(false));
    assert_eq!(end.counts(), (0, 0, 1));
    assert_eq!(run.get(), Stopping);
    assert!(!run.held());
}

#[test]
fn a_stop_cancels_a_paused_source_and_leaves_the_release_to_it() {
    let run = parked_source();
    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!(calls.counts(), (0, 1, 0), "released under the migration");
    assert!(run.cancelled());

    assert!(!run.commit(), "committed after a stop");
    assert_eq!(
        run.release_migration(
            || -> Result<(), ()> { panic!("released") },
            || true
        ),
        Err(None)
    );

    let end = Calls::default();
    assert_eq!(run.end_migration(|| end.note("release")), Ok(false));
    assert_eq!(end.counts(), (0, 0, 1));
    assert!(!run.held());
}

#[test]
fn a_stop_cancels_an_import_and_leaves_the_release_to_it() {
    // The import runs on a VM the operator paused, so its vCPUs are
    // parked although the state is not Paused.
    let run = RunControl::new(Paused);
    import(&run).expect("claim");
    let calls = Calls::default();

    stop(&run, &calls).expect("stop");

    assert_eq!(calls.counts(), (0, 1, 0));
    assert!(run.cancelled());
    assert!(!run.commit(), "committed after a stop");
    let end = Calls::default();
    assert_eq!(run.end_migration(|| end.note("release")), Ok(false));
    assert_eq!(end.counts(), (0, 0, 1));
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_second_stop_during_a_migration_releases_nothing() {
    // The watchdog's halt follows its poweroff.
    let run = RunControl::new(Paused);
    import(&run).expect("claim");
    stop(&run, &Calls::default()).expect("first");
    let second = Calls::default();
    stop(&run, &second).expect("second");
    assert_eq!(second.counts(), (0, 0, 0));
    assert!(run.held());
}

#[test]
fn a_stop_after_the_destination_commit_leaves_it_to_finish() {
    // The import still resumes the devices, so teardown has to wait.
    let run = RunControl::new(Paused);
    import(&run).expect("claim");
    assert!(run.commit());
    let calls = Calls::default();

    stop(&run, &calls).expect("stop");

    assert_eq!(calls.counts(), (0, 1, 0));
    assert!(!run.cancelled());
    assert_eq!(
        run.release_migration(
            || -> Result<(), ()> { panic!("released") },
            || true
        ),
        Err(None)
    );
    let end = Calls::default();
    assert_eq!(run.end_migration(|| end.note("release")), Ok(false));
    assert_eq!(end.counts(), (0, 0, 1));
}

#[test]
fn a_stop_after_the_source_commit_only_reaps_it() {
    let run = parked_source();
    assert!(run.commit());
    let calls = Calls::default();

    stop(&run, &calls).expect("stop");

    assert_eq!(calls.counts(), (0, 1, 1));
    assert!(!run.cancelled());
    let retry = Calls::default();
    assert_eq!(run.hand_off(|| retry.note("release")), Ok(false));
    assert_eq!(retry.counts(), (0, 0, 0), "released twice");
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_failed_latch_gives_back_the_pause_a_stop_took() {
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    let calls = Calls::default();

    let failed = run.stop(
        STOP_WAIT,
        || calls.note("hold"),
        || Err("EIO"),
        || calls.note("release"),
    );

    assert_eq!(failed, Err(StopFailure::NotLatched("EIO")));
    assert_eq!(calls.counts(), (1, 0, 1));
    assert_eq!(run.get(), Migrating);
    assert!(!run.held());
    assert!(!run.cancelled());
}

#[test]
fn a_finished_source_retries_a_release_its_stop_could_not_make() {
    let run = parked_source();
    assert!(run.commit());
    let failed = run.stop(STOP_WAIT, ok, ok, || Err(()));
    assert_eq!(failed, Err(StopFailure::NotReleased(())));
    assert!(run.held());
    assert!(run.must_reap());

    let retry = Calls::default();
    assert_eq!(run.hand_off(|| retry.note("release")), Ok(false));
    assert_eq!(retry.counts(), (0, 0, 1));
    assert!(!run.held());
    assert!(!run.must_reap());
}

#[test]
fn a_finished_source_stays_paused() {
    let run = parked_source();
    assert!(run.commit());
    assert_eq!(run.hand_off(ok), Ok(true));
    assert_eq!(run.get(), Stopped);
    assert!(run.held());
    assert!(run.must_reap());

    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!(calls.counts(), (0, 1, 1));
    assert!(!run.must_reap());
}

#[test]
fn only_a_migrated_source_is_reaped() {
    for state in [Running, Paused, Migrating, Faulted] {
        assert!(!RunControl::new(state).must_reap(), "{state:?}");
    }
}

#[test]
fn a_migration_ends_in_the_state_its_pause_says() {
    // A destination that failed before it resumed the guest.
    let run = RunControl::new(Paused);
    import(&run).expect("claim");
    run.park_migration(ok).expect("already held");
    assert_eq!(run.end_migration(ok), Ok(true));
    assert_eq!(run.get(), Paused);

    // One that resumed the guest.
    import(&run).expect("claim");
    run.release_migration(ok, || true).expect("released");
    assert_eq!(run.end_migration(ok), Ok(true));
    assert_eq!(run.get(), Running);

    // A source whose rollback could not resume the vCPUs, and parked
    // its devices again.
    let run = parked_source();
    assert_eq!(
        run.release_migration(|| Err("EBUSY"), || true),
        Err(Some("EBUSY"))
    );
    assert_eq!(run.end_migration(ok), Ok(true));
    assert_eq!(run.get(), Paused);
}

#[test]
fn a_migration_whose_devices_will_not_park_again_ends_faulted() {
    // Paused would let an import write into running devices.
    let run = parked_source();
    assert_eq!(
        run.release_migration(|| Err("EBUSY"), || false),
        Err(Some("EBUSY"))
    );
    assert_eq!(run.end_migration(ok), Ok(true));
    assert_eq!(run.get(), Faulted);
    assert!(run.held());
}

#[test]
fn an_operator_resume_that_cannot_park_again_faults_the_vm() {
    let run = RunControl::new(Paused);
    let failed =
        run.change(Paused, Running, |_| Err(StepFailed::Faulted("EBUSY")));
    assert_eq!(failed, Err(ChangeFailed::Faulted("EBUSY")));
    assert_eq!(run.get(), Faulted);
}

#[test]
fn a_faulted_vm_takes_only_a_stop() {
    let run = RunControl::new(Paused);
    let _ = run.change(Paused, Running, |_| Err(StepFailed::Faulted(())));
    assert_eq!(run.get(), Faulted);

    assert!(import(&run).is_err());
    assert!(run.admit_source(|| None).is_err());
    let never = |_: &mut bool| -> Result<(), StepFailed<()>> {
        panic!("a faulted VM changed")
    };
    assert_eq!(
        run.change(Running, Paused, never),
        Err(ChangeFailed::WrongState)
    );
    assert_eq!(
        run.change(Paused, Running, never),
        Err(ChangeFailed::WrongState)
    );

    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!(calls.counts(), (0, 1, 1));
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_failed_suspend_puts_back_the_state_it_took() {
    for from in [Running, Paused, Migrating, Stopped, Faulted] {
        let run = RunControl::new(from);
        let failed = run.stop(STOP_WAIT, ok, || Err(()), || panic!("resumed"));
        assert_eq!(failed, Err(StopFailure::NotLatched(())), "{from:?}");
        assert_eq!(run.get(), from);
    }
}

#[test]
fn a_second_stop_leaves_the_work_to_the_first() {
    let run = RunControl::new(Paused);
    let first = Calls::default();
    let second = Calls::default();
    stop(&run, &first).expect("first");
    stop(&run, &second).expect("second");
    assert_eq!(first.counts(), (0, 1, 1));
    assert_eq!(second.counts(), (0, 0, 0));
}

#[test]
fn a_later_stop_retries_a_release_the_first_could_not_make() {
    // The watchdog's halt follows its poweroff, so a second stop is
    // what gets a paused VM's vCPUs to the suspend after all.
    let run = RunControl::new(Paused);
    let failed = run.stop(STOP_WAIT, ok, ok, || Err(()));
    assert_eq!(failed, Err(StopFailure::NotReleased(())));

    let retry = Calls::default();
    stop(&run, &retry).expect("retried");
    assert_eq!(retry.counts(), (0, 0, 1));
    assert!(!run.held());
}

#[test]
fn a_panic_in_a_step_rolls_the_state_back() {
    let run = RunControl::new(Running);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _ =
            run.change(Running, Paused, |_| -> Result<(), StepFailed<()>> {
                panic!("step")
            });
    }));
    assert!(panicked.is_err());
    assert_eq!(run.get(), Running);
}

#[test]
fn a_panic_in_a_source_refusal_rolls_the_claim_back() {
    let run = RunControl::new(Running);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _ = run.admit_source(|| panic!("refusal"));
    }));
    assert!(panicked.is_err());
    assert_eq!(run.get(), Running);
}

#[test]
fn a_change_racing_a_stop_never_reports_the_wrong_state() {
    let run = RunControl::new(Running);
    let started = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !started.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            stop(&run, &Calls::default()).expect("stop");
        });
        let paused = run.change(Running, Paused, |held| {
            started.store(true, Ordering::Release);
            *held = true;
            step_ok()
        });
        assert_eq!(paused, Ok(()));
    });
    assert_eq!(run.get(), Stopping);
    assert!(!run.held());
}
