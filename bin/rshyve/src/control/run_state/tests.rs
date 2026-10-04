// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use VmRunState::{Migrating, Paused, Running, Stopped, Stopping};

fn import(run: &RunControl) -> Result<(), String> {
    run.admit_import(|| None)
}

fn ok() -> Result<(), ()> {
    Ok(())
}

/// Counts the calls a stop makes, so a test can say which ran.
#[derive(Default)]
struct Calls {
    latches: AtomicUsize,
    releases: AtomicUsize,
}

impl Calls {
    fn latch(&self) -> Result<(), &'static str> {
        self.latches.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn release(&self) -> Result<(), &'static str> {
        self.releases.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn latches(&self) -> usize {
        self.latches.load(Ordering::Acquire)
    }

    fn releases(&self) -> usize {
        self.releases.load(Ordering::Acquire)
    }
}

fn stop(
    run: &RunControl,
    calls: &Calls,
) -> Result<(), StopFailure<&'static str>> {
    run.stop(|| calls.latch(), || calls.release())
}

/// A source that got as far as its vCPU pause.
fn parked_source() -> RunControl {
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    run.park_migration(ok).expect("parked");
    run
}

#[test]
fn an_import_cannot_claim_a_vm_that_is_still_pausing() {
    // The state already says Paused while the devices are parking.
    let run = RunControl::new(Running);
    run.change(Running, Paused, |held| {
        assert_eq!(run.get(), Paused);
        assert_eq!(import(&run), Err(CHANGE_IN_PROGRESS.to_string()));
        *held = true;
        ok()
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
        ok()
    })
    .expect("resume");

    assert!(import(&run).is_err());
    assert_eq!(run.get(), Running);
}

#[test]
fn a_resume_after_an_import_claim_leaves_it_migrating() {
    let run = RunControl::new(Paused);
    import(&run).expect("claim");

    let resumed = run.change(Paused, Running, |_| -> Result<(), ()> {
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
        ok()
    })
    .expect("resume");
    run.admit_source(|| None).expect("running");
}

#[test]
fn a_rollback_leaves_a_stop_alone() {
    // The signal-driven stop takes a migrating VM to Stopping.
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    assert!(run.transition(Migrating, Stopping));

    assert_eq!(run.end_migration(ok), Ok(false));
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_stop_during_a_source_refusal_survives_it() {
    let run = RunControl::new(Running);
    let refused = run.admit_source(|| {
        assert!(run.transition(Migrating, Stopping));
        Some("grew".to_string())
    });
    assert!(refused.is_err());
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_failed_pause_rolls_back_but_not_over_a_stop() {
    let run = RunControl::new(Running);
    let failed = run.change(Running, Paused, |_| Err("stuck"));
    assert_eq!(failed, Err(ChangeFailed::Step("stuck")));
    assert_eq!(run.get(), Running);

    let failed = run.change(Running, Paused, |_| {
        assert!(run.transition(Paused, Stopping));
        Err("stuck")
    });
    assert_eq!(failed, Err(ChangeFailed::Step("stuck")));
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
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(calls.latches(), 0, "a stop ran inside a pause");
            *held = true;
            ok()
        })
        .expect("pause");
    });

    assert_eq!((calls.latches(), calls.releases()), (1, 1));
    assert_eq!(run.get(), Stopping);
    assert!(!run.held());
}

#[test]
fn a_pause_runs_under_the_topology_lock() {
    // A hot-add holds it from its Running check to its last step, so
    // the device set a pause parks is the whole set.
    let run = RunControl::new(Running);
    run.change(Running, Paused, |held| {
        assert!(run.topology.is_held(), "topology not held");
        *held = true;
        ok()
    })
    .expect("pause");
}

#[test]
fn a_finished_source_leaves_a_stop_in_charge() {
    let run = parked_source();
    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!(calls.releases(), 1, "the stop released the source");

    let retry = Calls::default();
    assert_eq!(run.hand_off(|| retry.release()), Ok(false));
    assert_eq!(run.get(), Stopping);
    assert_eq!(retry.releases(), 0, "released twice");
}

#[test]
fn a_finished_source_retries_a_release_its_stop_could_not_make() {
    let run = parked_source();
    let failed = run.stop(|| Ok(()), || Err("EBUSY"));
    assert_eq!(failed, Err(StopFailure::NotReleased("EBUSY")));
    assert!(run.held());

    let retry = Calls::default();
    assert_eq!(run.hand_off(|| retry.release()), Ok(false));
    assert_eq!(retry.releases(), 1);
    assert!(!run.held());
}

#[test]
fn a_finished_source_stays_paused() {
    let run = parked_source();
    assert_eq!(run.hand_off(ok), Ok(true));
    assert_eq!(run.get(), Stopped);
    assert!(run.held());

    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!((calls.latches(), calls.releases()), (1, 1));
}

#[test]
fn a_stop_releases_a_migrating_destination() {
    // The import runs on a VM the operator paused, so its vCPUs are
    // parked although the state is not Paused.
    let run = RunControl::new(Paused);
    import(&run).expect("claim");
    let calls = Calls::default();

    stop(&run, &calls).expect("stop");

    assert_eq!((calls.latches(), calls.releases()), (1, 1));
    assert!(!run.held());
    assert_eq!(run.end_migration(ok), Ok(false));
    assert_eq!(run.get(), Stopping);
}

#[test]
fn a_stop_releases_a_source_once_it_paused() {
    let run = parked_source();
    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!((calls.latches(), calls.releases()), (1, 1));
}

#[test]
fn a_stop_does_not_resume_a_source_still_running() {
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    let calls = Calls::default();
    stop(&run, &calls).expect("stop");
    assert_eq!((calls.latches(), calls.releases()), (1, 0));
}

#[test]
fn a_migration_may_not_pause_a_vm_a_stop_owns() {
    // The stop found nothing to release, so a pause taken now would
    // hold the vCPUs away from the suspend.
    let run = RunControl::new(Running);
    run.admit_source(|| None).expect("claim");
    stop(&run, &Calls::default()).expect("stop");

    let refused = run.park_migration(|| -> Result<(), ()> {
        panic!("paused a stopping VM")
    });

    assert_eq!(refused, Err(None));
    assert!(!run.held());
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
    run.unpark(ok).expect("released");
    assert_eq!(run.end_migration(ok), Ok(true));
    assert_eq!(run.get(), Running);

    // A source whose rollback could not resume the vCPUs.
    let run = parked_source();
    assert_eq!(run.unpark(|| Err("EBUSY")), Err("EBUSY"));
    assert_eq!(run.end_migration(ok), Ok(true));
    assert_eq!(run.get(), Paused);
}

#[test]
fn a_failed_suspend_puts_back_the_state_it_took() {
    for from in [Running, Paused, Migrating, Stopped] {
        let run = RunControl::new(from);
        let failed = run.stop(|| Err("EIO"), || panic!("resumed"));
        assert_eq!(failed, Err(StopFailure::NotLatched("EIO")), "{from:?}");
        assert_eq!(run.get(), from);
    }
}

#[test]
fn a_failed_suspend_rolls_back_by_compare_and_swap() {
    let run = RunControl::new(Running);
    let failed = run.stop(
        || {
            assert!(run.transition(Stopping, Stopped));
            Err("EIO")
        },
        || panic!("resumed"),
    );
    assert!(failed.is_err());
    assert_eq!(run.get(), Stopped);
}

#[test]
fn a_second_stop_leaves_the_work_to_the_first() {
    let run = RunControl::new(Paused);
    let first = Calls::default();
    let second = Calls::default();
    stop(&run, &first).expect("first");
    stop(&run, &second).expect("second");
    assert_eq!((first.latches(), first.releases()), (1, 1));
    assert_eq!((second.latches(), second.releases()), (0, 0));
}

#[test]
fn a_later_stop_retries_a_release_the_first_could_not_make() {
    // The watchdog's halt follows its poweroff, so a second stop is
    // what gets a paused VM's vCPUs to the suspend after all.
    let run = RunControl::new(Paused);
    let failed = run.stop(|| Ok(()), || Err("EBUSY"));
    assert_eq!(failed, Err(StopFailure::NotReleased("EBUSY")));

    let retry = Calls::default();
    stop(&run, &retry).expect("retried");
    assert_eq!((retry.latches(), retry.releases()), (0, 1));
    assert!(!run.held());
}

#[test]
fn a_panic_in_a_step_rolls_the_state_back() {
    let run = RunControl::new(Running);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _ = run
            .change(Running, Paused, |_| -> Result<(), ()> { panic!("step") });
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
            ok()
        });
        assert_eq!(paused, Ok(()));
    });
    assert_eq!(run.get(), Stopping);
    assert!(!run.held());
}
