// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The VM's run state and the locks that order changes to it.
//!
//! | State       | Physically                                           |
//! |-------------|------------------------------------------------------|
//! | `Running`   | vCPUs and devices run                                |
//! | `Paused`    | devices parked, pause held                           |
//! | `Migrating` | dest: as `Paused`; source: runs until its own pause  |
//! | `Stopped`   | a migrated-away source: devices parked, pause held   |
//! | `Stopping`  | suspend latched, pause given back                    |
//!
//! "Pause held" is this controller's hold on the kernel pause. Only a
//! caller under `changing` takes or gives it back, so a stop always
//! knows whether it has a pause to release. Every move into or out of
//! `Paused` and every stop runs under `changing`. A migration thread
//! takes it only for its pause, its release and its end, and ends with
//! a compare-and-swap so it never undoes a stop.
//!
//! Lock order is `changing`, then `topology`. Hot-add and eject take
//! only `topology`, and nothing under it takes `changing`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use vmm_machine::TopologyLock;

use super::VmRunState;

/// Why `migrate-source` or `migrate-dest` was refused while another
/// change held the VM.
pub(super) const CHANGE_IN_PROGRESS: &str =
    "another pause, resume, stop or migration is in progress; retry when \
     it ends";

/// Why a migration may not pause a VM a stop has claimed.
pub(super) const STOP_OWNS_VM: &str = "a stop arrived during the migration";

/// The run state, readable from any thread without a lock.
pub(super) struct RunControl {
    state: AtomicU8,
    /// Whether this controller holds the kernel pause.
    changing: Mutex<bool>,
    /// Held across every change to the device set, and across a pause
    /// or resume so neither sees a device arrive half way.
    topology: TopologyLock,
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

/// Puts `from` back if the change does not finish, a panic included.
struct Rollback<'a> {
    run: &'a RunControl,
    from: VmRunState,
    to: VmRunState,
}

impl Rollback<'_> {
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        self.run.transition(self.to, self.from);
    }
}

impl RunControl {
    #[cfg(test)]
    pub(super) fn new(state: VmRunState) -> Self {
        Self::with_topology(state, TopologyLock::default())
    }

    pub(super) fn with_topology(
        state: VmRunState,
        topology: TopologyLock,
    ) -> Self {
        Self {
            state: AtomicU8::new(state as u8),
            changing: Mutex::new(state == VmRunState::Paused),
            topology,
        }
    }

    pub(super) fn get(&self) -> VmRunState {
        VmRunState::from_u8(self.state.load(Ordering::Acquire))
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
        self.topology.lock()
    }

    #[cfg(test)]
    fn held(&self) -> bool {
        *self.lock_changing()
    }

    // One flag, which a panic cannot leave half written, so a poisoned
    // lock is taken rather than dropping a control request.
    fn lock_changing(&self) -> MutexGuard<'_, bool> {
        self.changing.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_changing(&self) -> Option<MutexGuard<'_, bool>> {
        match self.changing.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Run an operator pause or resume.
    ///
    /// `step` gets the pause-held flag and must leave it true exactly
    /// when the kernel pause is ours. On failure the state goes back to
    /// `from` only if the VM is still in `to`.
    pub(super) fn change<E>(
        &self,
        from: VmRunState,
        to: VmRunState,
        step: impl FnOnce(&mut bool) -> Result<(), E>,
    ) -> Result<(), ChangeFailed<E>> {
        let mut held = self.lock_changing();
        let _topology = self.lock_topology();
        if !self.transition(from, to) {
            return Err(ChangeFailed::WrongState);
        }
        let rollback = Rollback {
            run: self,
            from,
            to,
        };
        step(&mut held).map_err(ChangeFailed::Step)?;
        rollback.disarm();
        Ok(())
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
        let rollback = Rollback {
            run: self,
            from: VmRunState::Running,
            to: VmRunState::Migrating,
        };
        let _topology = self.lock_topology();
        match refusal() {
            Some(why) => Err(why),
            None => {
                rollback.disarm();
                Ok(())
            }
        }
    }

    /// Claim a paused VM for a migration import.
    ///
    /// The import writes every vCPU's registers and every device's
    /// rings, so the VM must be paused and stay paused. The locks keep
    /// two imports apart; the compare-and-swap keeps out a stop that
    /// moved the VM on without them.
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

    /// Take the kernel pause for a migration. `Err(None)` when a stop
    /// owns the VM: it found no pause to release, so one taken now would
    /// keep the vCPUs from its suspend.
    pub(super) fn park_migration<E>(
        &self,
        hold: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), Option<E>> {
        let mut held = self.lock_changing();
        if self.get() != VmRunState::Migrating {
            return Err(None);
        }
        if !*held {
            hold().map_err(Some)?;
            *held = true;
        }
        Ok(())
    }

    /// Give back the kernel pause, if this controller holds it.
    pub(super) fn unpark<E>(
        &self,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        let mut held = self.lock_changing();
        give_back(&mut held, release)
    }

    /// End a migration's claim in the state its pause says: `Paused`
    /// while the pause is held, `Running` once it is not.
    ///
    /// False when a stop owns the VM. `release` then retries a release
    /// that stop could not make; its suspend is latched, so the vCPUs
    /// go straight to the exit.
    pub(super) fn end_migration<E>(
        &self,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut held = self.lock_changing();
        let to = if *held {
            VmRunState::Paused
        } else {
            VmRunState::Running
        };
        if self.transition(VmRunState::Migrating, to) {
            return Ok(true);
        }
        self.release_for_stop(&mut held, release).map(|()| false)
    }

    /// Mark a source whose guest now runs elsewhere. The pause stays
    /// held so the guest never runs here again.
    ///
    /// False, with the same retry as [`Self::end_migration`], when a
    /// stop owns the VM.
    pub(super) fn hand_off<E>(
        &self,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut held = self.lock_changing();
        if self.transition(VmRunState::Migrating, VmRunState::Stopped) {
            return Ok(true);
        }
        self.release_for_stop(&mut held, release).map(|()| false)
    }

    fn release_for_stop<E>(
        &self,
        held: &mut bool,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        if self.get() == VmRunState::Stopping {
            give_back(held, release)?;
        }
        Ok(())
    }

    /// Latch a stop and let a parked VM's vCPUs reach it.
    ///
    /// Under `changing`, so a pause in progress finishes first and the
    /// stop sees the pause it took. A stop that finds `Stopping` only
    /// retries a release the first one could not make.
    pub(super) fn stop<E>(
        &self,
        latch: impl FnOnce() -> Result<(), E>,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), StopFailure<E>> {
        let mut held = self.lock_changing();
        let Some(from) = self.claim_stop() else {
            return give_back(&mut held, release)
                .map_err(StopFailure::NotReleased);
        };
        let result = if *held {
            release_paused_vm(latch, || give_back(&mut held, release))
        } else {
            latch().map_err(StopFailure::NotLatched)
        };
        if let Err(StopFailure::NotLatched(_)) = &result {
            self.transition(VmRunState::Stopping, from);
        }
        result
    }

    /// The state the stop took the VM from, or `None` when another stop
    /// already holds it.
    fn claim_stop(&self) -> Option<VmRunState> {
        let mut seen = self.state.load(Ordering::Acquire);
        loop {
            if VmRunState::from_u8(seen) == VmRunState::Stopping {
                return None;
            }
            match self.state.compare_exchange(
                seen,
                VmRunState::Stopping as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(from) => return Some(VmRunState::from_u8(from)),
                Err(now) => seen = now,
            }
        }
    }
}

fn give_back<E>(
    held: &mut bool,
    release: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    if *held {
        release()?;
        *held = false;
    }
    Ok(())
}

/// Stop a paused VM without letting its guest run one more instruction.
///
/// The suspend is latched FIRST. A paused vCPU thread is retrying
/// VM_RUN against EBUSY; the resume lets it back into the kernel, which
/// sees the latched suspend and turns it straight around. Resuming
/// first would run guest code against a disk the migration destination
/// already owns. A suspend that fails is not followed by a resume, for
/// the same reason.
pub(super) fn release_paused_vm<E>(
    suspend: impl FnOnce() -> Result<(), E>,
    resume: impl FnOnce() -> Result<(), E>,
) -> Result<(), StopFailure<E>> {
    suspend().map_err(StopFailure::NotLatched)?;
    resume().map_err(StopFailure::NotReleased)
}

/// Which step of a stop failed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StopFailure<E> {
    /// The suspend did not latch, so the VM is as it was.
    NotLatched(E),
    /// The suspend latched, but the parked VM was not let back into the
    /// kernel, so its vCPUs have not seen the stop yet.
    NotReleased(E),
}

impl<E> StopFailure<E> {
    pub(super) fn into_inner(self) -> E {
        match self {
            StopFailure::NotLatched(e) | StopFailure::NotReleased(e) => e,
        }
    }
}

impl From<StopFailure<std::io::Error>> for std::io::Error {
    fn from(failure: StopFailure<std::io::Error>) -> Self {
        failure.into_inner()
    }
}

#[cfg(test)]
mod tests;
