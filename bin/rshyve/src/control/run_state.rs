// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The VM's run state and the locks that order changes to it.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use super::VmRunState;

/// Why `migrate-source` or `migrate-dest` was refused while an operator
/// pause or resume was still under way.
pub(super) const CHANGE_IN_PROGRESS: &str =
    "a pause or resume is in progress; retry when it ends";

/// The run state, readable from any thread without a lock.
///
/// `pause` and `resume` publish their new state before the devices and
/// vCPUs reach it, so a hot-add sees the change at once. The `changing`
/// lock is held across the whole of each one, and a migration takes it
/// before it claims the VM. A migration therefore never sees a state
/// the devices have not reached.
///
/// Lock order is `changing`, then `topology`.
pub(super) struct RunControl {
    state: AtomicU8,
    changing: Mutex<()>,
    /// Held by a hot-add for its whole run.
    topology: Mutex<()>,
}

/// How a pause or resume ended, when it did not.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChangeFailed<E> {
    /// The VM was not in the state the change starts from.
    WrongState,
    /// The step failed. The state is back where it was, unless a stop
    /// took the VM over first.
    Step(E),
}

impl RunControl {
    pub(super) fn new(state: VmRunState) -> Self {
        Self {
            state: AtomicU8::new(state as u8),
            changing: Mutex::new(()),
            topology: Mutex::new(()),
        }
    }

    pub(super) fn get(&self) -> VmRunState {
        VmRunState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Only for a caller that owns the current state, such as the stop
    /// that moved the VM to `Stopping`.
    pub(super) fn set(&self, state: VmRunState) {
        self.state.store(state as u8, Ordering::Release);
    }

    pub(super) fn atomic(&self) -> &AtomicU8 {
        &self.state
    }

    pub(super) fn transition(&self, from: VmRunState, to: VmRunState) -> bool {
        self.state
            .compare_exchange(
                from as u8,
                to as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(super) fn lock_topology(&self) -> MutexGuard<'_, ()> {
        // Both locks guard a unit, which a panic cannot leave half
        // written, so a poisoned one is taken rather than dropping a
        // control request.
        self.topology.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_changing(&self) -> Option<MutexGuard<'_, ()>> {
        match self.changing.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Run an operator pause or resume.
    ///
    /// `to` is published before `step` runs, and on failure the state
    /// goes back to `from` only if the VM is still in `to`: a stop that
    /// took it over meanwhile must not be undone.
    pub(super) fn change<E>(
        &self,
        from: VmRunState,
        to: VmRunState,
        step: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), ChangeFailed<E>> {
        let _changing = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        if !self.transition(from, to) {
            return Err(ChangeFailed::WrongState);
        }
        step().map_err(|e| {
            self.transition(to, from);
            ChangeFailed::Step(e)
        })
    }

    /// Claim a running VM as a migration source.
    ///
    /// The state moves first and the refusal runs under the topology
    /// lock after it. An add already in flight is then counted, and one
    /// that comes later finds the VM migrating.
    pub(super) fn admit_source(
        &self,
        refusal: impl FnOnce() -> Option<String>,
    ) -> Result<(), String> {
        let Some(_changing) = self.try_changing() else {
            return Err(CHANGE_IN_PROGRESS.to_string());
        };
        if !self.transition(VmRunState::Running, VmRunState::Migrating) {
            return Err("VM must be running to start migration".to_string());
        }
        let refused = {
            let _topology = self.lock_topology();
            refusal()
        };
        match refused {
            Some(why) => {
                self.end_migration(VmRunState::Running);
                Err(why)
            }
            None => Ok(()),
        }
    }

    /// Claim a paused VM for a migration import.
    ///
    /// The import writes every vCPU's registers and every device's
    /// rings, so the VM must be paused and stay paused. The claim is a
    /// compare-and-swap so two imports cannot both pass.
    pub(super) fn admit_import(
        &self,
        refusal: impl FnOnce() -> Option<String>,
    ) -> Result<(), String> {
        const NOT_PAUSED: &str = "VM must be paused to receive migration";
        let Some(_changing) = self.try_changing() else {
            return Err(CHANGE_IN_PROGRESS.to_string());
        };
        let _topology = self.lock_topology();
        if self.get() != VmRunState::Paused {
            return Err(NOT_PAUSED.to_string());
        }
        if let Some(why) = refusal() {
            return Err(why);
        }
        if !self.transition(VmRunState::Paused, VmRunState::Migrating) {
            return Err(NOT_PAUSED.to_string());
        }
        Ok(())
    }

    /// Give up a migration's claim. False when the VM is no longer
    /// migrating, which is a stop that arrived meanwhile and owns the
    /// state now.
    pub(super) fn end_migration(&self, to: VmRunState) -> bool {
        self.transition(VmRunState::Migrating, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn import(run: &RunControl) -> Result<(), String> {
        run.admit_import(|| None)
    }

    #[test]
    fn an_import_cannot_claim_a_vm_that_is_still_pausing() {
        // The state already says Paused while the devices are parking.
        let run = RunControl::new(VmRunState::Running);
        run.change(VmRunState::Running, VmRunState::Paused, || {
            assert_eq!(run.get(), VmRunState::Paused);
            assert_eq!(import(&run), Err(CHANGE_IN_PROGRESS.to_string()));
            Ok::<(), ()>(())
        })
        .expect("pause");

        import(&run).expect("fully paused");
        assert_eq!(run.get(), VmRunState::Migrating);
    }

    #[test]
    fn an_import_cannot_claim_a_vm_that_is_resuming() {
        let run = RunControl::new(VmRunState::Paused);
        run.change(VmRunState::Paused, VmRunState::Running, || {
            assert_eq!(import(&run), Err(CHANGE_IN_PROGRESS.to_string()));
            Ok::<(), ()>(())
        })
        .expect("resume");

        assert!(import(&run).is_err());
        assert_eq!(run.get(), VmRunState::Running);
    }

    #[test]
    fn a_resume_after_an_import_claim_leaves_it_migrating() {
        let run = RunControl::new(VmRunState::Paused);
        import(&run).expect("claim");

        let resumed =
            run.change(VmRunState::Paused, VmRunState::Running, || {
                panic!("a resume must not touch a migrating VM")
            });

        assert_eq!(resumed, Err(ChangeFailed::<()>::WrongState));
        assert_eq!(run.get(), VmRunState::Migrating);
    }

    #[test]
    fn a_second_import_is_refused() {
        let run = RunControl::new(VmRunState::Paused);
        import(&run).expect("first");
        assert!(import(&run).is_err());
        assert_eq!(run.get(), VmRunState::Migrating);
    }

    #[test]
    fn of_racing_imports_exactly_one_wins() {
        for _ in 0..200 {
            let run = Arc::new(RunControl::new(VmRunState::Paused));
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
        let run = RunControl::new(VmRunState::Paused);
        let refused = run.admit_import(|| Some("blocked".to_string()));
        assert_eq!(refused, Err("blocked".to_string()));
        assert_eq!(run.get(), VmRunState::Paused);
    }

    #[test]
    fn a_source_refusal_runs_under_the_topology_lock_and_rolls_back() {
        let run = RunControl::new(VmRunState::Running);
        let refused = run.admit_source(|| {
            assert_eq!(run.get(), VmRunState::Migrating);
            assert!(run.topology.try_lock().is_err(), "topology not held");
            Some("grew".to_string())
        });
        assert_eq!(refused, Err("grew".to_string()));
        assert_eq!(run.get(), VmRunState::Running);
    }

    #[test]
    fn an_import_refusal_runs_under_the_topology_lock() {
        let run = RunControl::new(VmRunState::Paused);
        run.admit_import(|| {
            assert!(run.topology.try_lock().is_err(), "topology not held");
            None
        })
        .expect("admitted");
    }

    #[test]
    fn a_source_cannot_claim_a_vm_that_is_resuming() {
        let run = RunControl::new(VmRunState::Paused);
        run.change(VmRunState::Paused, VmRunState::Running, || {
            let claimed = run.admit_source(|| None);
            assert_eq!(claimed, Err(CHANGE_IN_PROGRESS.to_string()));
            Ok::<(), ()>(())
        })
        .expect("resume");
        run.admit_source(|| None).expect("running");
    }

    #[test]
    fn a_rollback_leaves_a_stop_alone() {
        // The signal-driven stop takes a migrating VM to Stopping.
        let run = RunControl::new(VmRunState::Running);
        run.admit_source(|| None).expect("claim");
        assert!(run.transition(VmRunState::Migrating, VmRunState::Stopping));

        assert!(!run.end_migration(VmRunState::Running));
        assert_eq!(run.get(), VmRunState::Stopping);
    }

    #[test]
    fn a_stop_during_a_source_refusal_survives_it() {
        let run = RunControl::new(VmRunState::Running);
        let refused =
            run.admit_source(|| {
                assert!(
                    run.transition(VmRunState::Migrating, VmRunState::Stopping)
                );
                Some("grew".to_string())
            });
        assert!(refused.is_err());
        assert_eq!(run.get(), VmRunState::Stopping);
    }

    #[test]
    fn a_failed_pause_rolls_back_but_not_over_a_stop() {
        let run = RunControl::new(VmRunState::Running);
        let failed =
            run.change(VmRunState::Running, VmRunState::Paused, || {
                Err("stuck")
            });
        assert_eq!(failed, Err(ChangeFailed::Step("stuck")));
        assert_eq!(run.get(), VmRunState::Running);

        let failed =
            run.change(VmRunState::Running, VmRunState::Paused, || {
                assert!(
                    run.transition(VmRunState::Paused, VmRunState::Stopping)
                );
                Err("stuck")
            });
        assert_eq!(failed, Err(ChangeFailed::Step("stuck")));
        assert_eq!(run.get(), VmRunState::Stopping);
    }
}
