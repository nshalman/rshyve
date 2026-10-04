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
//! | `Stopping`  | suspend latched; the pause may still be held         |
//! | `Faulted`   | pause held, devices may run; only a stop is taken    |
//!
//! "Pause held" is this controller's hold on the kernel pause. Only a
//! caller under `changing` takes or gives it back, so a stop always
//! knows whether it has a pause to release. Every state change runs
//! under `changing`.
//!
//! While a migration thread is live, a stop latches the suspend and
//! leaves the pause held, so teardown cannot start under the thread;
//! the thread gives the pause back when it ends. Before the commit the
//! stop also cancels the migration, and the commit checks for that
//! under `changing`.
//!
//! Lock order is `changing`, then `topology`, then the pause gate.
//! Hot-add and eject take only `topology` and the gate, and nothing
//! under either takes `changing`.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use vmm_machine::TopologyLock;

use super::VmRunState;

/// Why `migrate-source` or `migrate-dest` was refused while another
/// change held the VM.
pub(super) const CHANGE_IN_PROGRESS: &str =
    "another pause, resume, stop or migration is in progress; retry when \
     it ends";

/// Why a migration may not pause or resume a VM a stop has claimed.
pub(super) const STOP_OWNS_VM: &str = "a stop arrived during the migration";

/// How long a stop waits for a change in progress. The SIGTERM
/// watchdog calls the stop, and its budgets do not run while it waits.
pub(super) const STOP_WAIT: Duration = Duration::from_secs(10);

const STOP_POLL: Duration = Duration::from_millis(2);

/// The run state, readable from any thread without a lock.
pub(super) struct RunControl {
    state: AtomicU8,
    changing: Mutex<Hold>,
    /// Set under `changing` by a stop that beat the commit. Read
    /// without it, so a migration can give up early.
    cancel: AtomicBool,
    /// Held across every change to the device set, and across a pause
    /// or resume so neither sees a device arrive half way.
    topology: TopologyLock,
    /// How often a stop found `changing` taken.
    #[cfg(test)]
    contended: std::sync::atomic::AtomicUsize,
}

/// What `changing` guards.
#[derive(Debug, Default)]
struct Hold {
    /// This controller holds the kernel pause.
    held: bool,
    /// A migration thread that may still touch the devices or the
    /// pause.
    migration: Option<Role>,
    committed: bool,
    /// A release failed and the devices could not be parked again.
    faulted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Source,
    Dest,
}

/// How a pause or resume ended, when it did not.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChangeFailed<E> {
    /// The VM was not in the state the change starts from.
    WrongState,
    /// The step failed. The state is back where it was.
    Step(E),
    /// The step failed half way. The VM is `Faulted`.
    Faulted(E),
}

/// How a pause or resume step failed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StepFailed<E> {
    /// Nothing changed.
    Undone(E),
    /// The devices run and the pause is still held.
    Faulted(E),
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
            changing: Mutex::new(Hold {
                held: state == VmRunState::Paused,
                ..Hold::default()
            }),
            cancel: AtomicBool::new(false),
            topology,
            #[cfg(test)]
            contended: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn get(&self) -> VmRunState {
        VmRunState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn transition(&self, from: VmRunState, to: VmRunState) -> bool {
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
        self.lock_changing().held
    }

    // A few flags, which a panic cannot leave half written, so a
    // poisoned lock is taken rather than dropping a control request.
    fn lock_changing(&self) -> MutexGuard<'_, Hold> {
        self.changing.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_changing(&self) -> Option<MutexGuard<'_, Hold>> {
        match self.changing.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    fn lock_changing_within(
        &self,
        wait: Duration,
    ) -> Option<MutexGuard<'_, Hold>> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(guard) = self.try_changing() {
                return Some(guard);
            }
            #[cfg(test)]
            self.contended.fetch_add(1, Ordering::AcqRel);
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(STOP_POLL);
        }
    }

    /// Run an operator pause or resume.
    ///
    /// `step` gets the pause-held flag and must leave it true exactly
    /// when the kernel pause is ours.
    pub(super) fn change<E>(
        &self,
        from: VmRunState,
        to: VmRunState,
        step: impl FnOnce(&mut bool) -> Result<(), StepFailed<E>>,
    ) -> Result<(), ChangeFailed<E>> {
        let mut hold = self.lock_changing();
        let _topology = self.lock_topology();
        if !self.transition(from, to) {
            return Err(ChangeFailed::WrongState);
        }
        let rollback = Rollback {
            run: self,
            from,
            to,
        };
        match step(&mut hold.held) {
            Ok(()) => {
                rollback.disarm();
                Ok(())
            }
            Err(StepFailed::Undone(e)) => Err(ChangeFailed::Step(e)),
            Err(StepFailed::Faulted(e)) => {
                rollback.disarm();
                self.transition(to, VmRunState::Faulted);
                Err(ChangeFailed::Faulted(e))
            }
        }
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
        let Some(mut hold) = self.try_changing() else {
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
        let topology = self.lock_topology();
        if let Some(why) = refusal() {
            return Err(why);
        }
        rollback.disarm();
        self.open_migration(&mut hold, &topology, Role::Source);
        Ok(())
    }

    /// Claim a paused VM for a migration import.
    ///
    /// The import writes every vCPU's registers and every device's
    /// rings, so the VM must be paused and stay paused, with the device
    /// set it was admitted with.
    pub(super) fn admit_import(
        &self,
        refusal: impl FnOnce() -> Option<String>,
    ) -> Result<(), String> {
        const NOT_PAUSED: &str = "VM must be paused to receive migration";
        let Some(mut hold) = self.try_changing() else {
            return Err(CHANGE_IN_PROGRESS.to_string());
        };
        let topology = self.lock_topology();
        if self.get() != VmRunState::Paused {
            return Err(NOT_PAUSED.to_string());
        }
        if let Some(why) = refusal() {
            return Err(why);
        }
        if !self.transition(VmRunState::Paused, VmRunState::Migrating) {
            return Err(NOT_PAUSED.to_string());
        }
        self.open_migration(&mut hold, &topology, Role::Dest);
        Ok(())
    }

    fn open_migration(
        &self,
        hold: &mut Hold,
        topology: &MutexGuard<'_, ()>,
        role: Role,
    ) {
        hold.migration = Some(role);
        hold.committed = false;
        hold.faulted = false;
        self.cancel.store(false, Ordering::Release);
        self.topology.hold_ejects(topology, true);
    }

    fn close_migration(&self, hold: &mut Hold) {
        hold.migration = None;
        self.topology.hold_ejects(&self.lock_topology(), false);
    }

    /// Whether a stop has cancelled the migration.
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Take the kernel pause for a migration. `Err(None)` when a stop
    /// owns the VM.
    pub(super) fn park_migration<E>(
        &self,
        hold: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), Option<E>> {
        let mut state = self.lock_changing();
        if self.get() != VmRunState::Migrating {
            return Err(None);
        }
        if !state.held {
            hold().map_err(Some)?;
            state.held = true;
        }
        Ok(())
    }

    /// Claim the step this side cannot take back. False when a stop
    /// cancelled the migration first; a stop after it only reaps.
    pub(super) fn commit(&self) -> bool {
        let mut hold = self.lock_changing();
        if self.cancelled() || self.get() != VmRunState::Migrating {
            return false;
        }
        hold.committed = true;
        // A committed source never touches the VM again, so a stop may
        // release it at once. A destination still has to start it.
        if hold.migration == Some(Role::Source) {
            hold.migration = None;
        }
        true
    }

    /// Give back a migration's pause, after the devices were resumed.
    ///
    /// `Err(None)` when a stop owns the VM: its suspend is latched and
    /// the migration's end lets the vCPUs reach it. When the release
    /// fails, `repark` puts the devices back; if it cannot, the
    /// migration ends `Faulted`.
    pub(super) fn release_migration<E>(
        &self,
        release: impl FnOnce() -> Result<(), E>,
        repark: impl FnOnce() -> bool,
    ) -> Result<(), Option<E>> {
        let mut hold = self.lock_changing();
        if self.get() != VmRunState::Migrating {
            return Err(None);
        }
        if !hold.held {
            return Ok(());
        }
        if let Err(e) = release() {
            if !repark() {
                hold.faulted = true;
            }
            return Err(Some(e));
        }
        hold.held = false;
        hold.faulted = false;
        Ok(())
    }

    /// End a migration's claim in the state its pause says: `Paused`
    /// while the pause is held, `Running` once it is not, and `Faulted`
    /// when neither holds.
    ///
    /// False when a stop owns the VM. `release` then gives back the
    /// pause the stop left for this end; the suspend is latched, so the
    /// vCPUs go straight to the exit.
    pub(super) fn end_migration<E>(
        &self,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut hold = self.lock_changing();
        self.close_migration(&mut hold);
        let to = if hold.faulted {
            VmRunState::Faulted
        } else if hold.held {
            VmRunState::Paused
        } else {
            VmRunState::Running
        };
        if self.transition(VmRunState::Migrating, to) {
            return Ok(true);
        }
        self.release_for_stop(&mut hold, release).map(|()| false)
    }

    /// Mark a source whose guest now runs elsewhere. The pause stays
    /// held so the guest never runs here again.
    ///
    /// False, with the same release as [`Self::end_migration`], when a
    /// stop owns the VM.
    pub(super) fn hand_off<E>(
        &self,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut hold = self.lock_changing();
        self.close_migration(&mut hold);
        if self.transition(VmRunState::Migrating, VmRunState::Stopped) {
            return Ok(true);
        }
        self.release_for_stop(&mut hold, release).map(|()| false)
    }

    fn release_for_stop<E>(
        &self,
        hold: &mut Hold,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        if self.get() == VmRunState::Stopping {
            give_back(&mut hold.held, release)?;
        }
        Ok(())
    }

    /// Whether a migrated-away source still needs its release: nothing
    /// stopped it yet, or a stop latched and could not give the pause
    /// back.
    pub(super) fn must_reap(&self) -> bool {
        let hold = self.lock_changing();
        match self.get() {
            VmRunState::Stopped => true,
            VmRunState::Stopping => hold.held && hold.migration.is_none(),
            _ => false,
        }
    }

    /// Latch a stop and let a parked VM's vCPUs reach it.
    ///
    /// Waits up to `wait` for a change in progress. A stop that finds
    /// `Stopping` only retries a release the first one could not make.
    /// See the module docs for a stop during a migration; `hold` is
    /// the pause it takes on a source whose vCPUs still run.
    pub(super) fn stop<E>(
        &self,
        wait: Duration,
        hold: impl FnOnce() -> Result<(), E>,
        latch: impl FnOnce() -> Result<(), E>,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), StopFailure<E>> {
        let Some(mut state) = self.lock_changing_within(wait) else {
            return Err(StopFailure::Busy);
        };
        let Some(from) = self.claim_stop() else {
            if state.migration.is_some() {
                return Ok(());
            }
            return give_back(&mut state.held, release)
                .map_err(StopFailure::NotReleased);
        };
        if state.migration.is_some() {
            return self.stop_migration(&mut state, from, hold, latch, release);
        }
        let result = if state.held {
            release_paused_vm(latch, || give_back(&mut state.held, release))
        } else {
            latch().map_err(StopFailure::NotLatched)
        };
        if let Err(StopFailure::NotLatched(_)) = &result {
            self.transition(VmRunState::Stopping, from);
        }
        result
    }

    fn stop_migration<E>(
        &self,
        state: &mut Hold,
        from: VmRunState,
        hold: impl FnOnce() -> Result<(), E>,
        latch: impl FnOnce() -> Result<(), E>,
        release: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), StopFailure<E>> {
        let took = !state.held;
        if took {
            if let Err(e) = hold() {
                self.transition(VmRunState::Stopping, from);
                return Err(StopFailure::NotLatched(e));
            }
            state.held = true;
        }
        if let Err(e) = latch() {
            if took {
                match release() {
                    Ok(()) => state.held = false,
                    // The migration rolls the devices back and the
                    // release fails again, or this does.
                    Err(_) => state.faulted = true,
                }
            }
            self.transition(VmRunState::Stopping, from);
            return Err(StopFailure::NotLatched(e));
        }
        if !state.committed {
            self.cancel.store(true, Ordering::Release);
        }
        Ok(())
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
    /// Another change held the VM past the stop's wait.
    Busy,
    /// The suspend did not latch, so the VM is as it was.
    NotLatched(E),
    /// The suspend latched, but the parked VM was not let back into the
    /// kernel, so its vCPUs have not seen the stop yet.
    NotReleased(E),
}

impl From<StopFailure<std::io::Error>> for std::io::Error {
    fn from(failure: StopFailure<std::io::Error>) -> Self {
        match failure {
            StopFailure::Busy => std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                CHANGE_IN_PROGRESS,
            ),
            StopFailure::NotLatched(e) | StopFailure::NotReleased(e) => e,
        }
    }
}

#[cfg(test)]
mod tests;
