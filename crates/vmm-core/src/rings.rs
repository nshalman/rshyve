// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one way into an ioctl that takes the VM write lock.
//!
//! The kernel takes a viona ring lock under the VM write lock: taking
//! the write lock expires every lease, and the expiry callback takes
//! the ring lock. A ring worker renews its lease with its ring lock
//! held, and the renewal waits for the write lock. If both happen at
//! once, neither side moves, the process cannot be killed, and only a
//! host reboot clears it. A paused worker is still alive and still
//! renews, so only a worker that has exited is safe.
//!
//! [`RingGate::run`] stops every member's workers, runs the closure,
//! and starts them again. The closure gets a [`RingsStopped`], and
//! every write-lock method on [`crate::hdl::VmmHdl`] asks for one, so
//! there is no other way to reach those ioctls.
//!
//! [`RingGate::hold`] keeps the workers stopped across many calls, for
//! a VM pause. A `run` under a hold does not stop anything again.
//!
//! Lock order: the gate, then a member's own lock. A member never
//! calls the gate with its own lock held, and never calls the gate
//! from inside [`KernelRings`]; the gate refuses the second entry on
//! one thread rather than deadlock on it.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// A device whose kernel workers hold a lease on the VM.
pub trait KernelRings: Send + Sync {
    /// Stop every kernel worker this device runs, and keep any from
    /// starting again until [`Self::start`].
    ///
    /// On an error the device must be left running as it was.
    fn stop(&self) -> Result<(), String>;

    /// Put back what [`Self::stop`] stopped.
    fn start(&self);
}

/// Why the gate did not run what it was given. Nothing ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// A device would not stop its rings. Every device it had stopped
    /// is running again.
    Busy(String),
    /// The gate was entered again from inside itself.
    Reentered,
    /// A member started a worker it could not stop again. Nothing may
    /// take the write lock on this VM any more.
    Tainted,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(why) => {
                write!(f, "the kernel rings did not stop: {why}")
            }
            Self::Reentered => f.write_str("the ring gate was entered twice"),
            Self::Tainted => {
                f.write_str("a kernel ring worker could not be stopped")
            }
        }
    }
}

impl std::error::Error for GateError {}

impl From<GateError> for std::io::Error {
    fn from(e: GateError) -> Self {
        let kind = match e {
            GateError::Busy(_) => std::io::ErrorKind::ResourceBusy,
            GateError::Reentered => std::io::ErrorKind::Deadlock,
            GateError::Tainted => std::io::ErrorKind::ResourceBusy,
        };
        Self::new(kind, e)
    }
}

/// Proof that no kernel ring worker of one VM is alive.
///
/// Only [`RingGate::run`] makes one, and only for the length of the
/// closure it runs.
pub struct RingsStopped<'a> {
    gate: &'a RingGate,
}

/// Every kernel ring of one VM, and who may stop them.
#[derive(Default)]
pub struct RingGate {
    state: Mutex<State>,
    /// Outside the lock, so a member inside [`RingGate::exclusive`] can
    /// set it.
    tainted: AtomicBool,
    /// Why the gate is tainted, for the VM status.
    taint_why: Mutex<Option<String>>,
    /// Devices left in the kernel because their rings could not be
    /// stopped for the delete.
    leaked: AtomicU32,
}

#[derive(Default)]
struct State {
    members: Vec<Weak<dyn KernelRings>>,
    holds: u32,
}

impl State {
    fn live(&mut self) -> Vec<Arc<dyn KernelRings>> {
        self.members.retain(|m| m.strong_count() > 0);
        self.members.iter().filter_map(Weak::upgrade).collect()
    }
}

thread_local! {
    // Some clippy releases flag a `const` initializer as not const.
    #[allow(clippy::missing_const_for_thread_local)]
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread as inside the gate until dropped.
struct Inside;

impl Inside {
    fn enter() -> Result<Self, GateError> {
        if INSIDE.with(|c| c.replace(true)) {
            return Err(GateError::Reentered);
        }
        Ok(Self)
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        INSIDE.with(|c| c.set(false));
    }
}

/// Stop every member, or none: a refusal starts again the ones already
/// stopped.
fn stop_all(members: &[Arc<dyn KernelRings>]) -> Result<(), GateError> {
    for (i, member) in members.iter().enumerate() {
        if let Err(why) = member.stop() {
            for done in members[..i].iter().rev() {
                done.start();
            }
            return Err(GateError::Busy(why));
        }
    }
    Ok(())
}

fn start_all(members: &[Arc<dyn KernelRings>]) {
    for member in members {
        member.start();
    }
}

/// Starts the members when dropped, so a closure that panics does not
/// leave the rings stopped.
struct Restart(Vec<Arc<dyn KernelRings>>);

impl Drop for Restart {
    fn drop(&mut self) {
        start_all(&self.0);
    }
}

impl RingGate {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The state is a list and a count, which a panic cannot leave half
    /// written, and a VM must still be able to pause after one.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the gate's lock, unless a worker escaped it.
    ///
    /// The taint is read under the lock: a member sets it from inside
    /// [`Self::exclusive`], and a caller that waited behind that has to
    /// see it.
    fn lock_open(&self) -> Result<MutexGuard<'_, State>, GateError> {
        let state = self.lock();
        if self.tainted.load(Ordering::Acquire) {
            return Err(GateError::Tainted);
        }
        Ok(state)
    }

    /// Say that a worker this gate counts as stopped may be alive.
    ///
    /// Every later run, hold and exclusive section is refused: under a
    /// hold a run stops nothing, so it would take the write lock with
    /// that worker alive. The caller logs it; the status shows `why`.
    pub fn taint(&self, why: &str) {
        let mut slot = self.taint_why.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(|| why.to_string());
        self.tainted.store(true, Ordering::Release);
    }

    /// Why the gate is tainted, or `None` while it is not.
    pub fn tainted(&self) -> Option<String> {
        if !self.tainted.load(Ordering::Acquire) {
            return None;
        }
        let why = self.taint_why.lock().unwrap_or_else(|e| e.into_inner());
        Some(why.clone().unwrap_or_default())
    }

    /// Count a device left in the kernel because its rings could not
    /// be stopped to delete it.
    pub fn note_leak(&self) {
        self.leaked.fetch_add(1, Ordering::AcqRel);
    }

    /// How many devices [`Self::note_leak`] counted.
    pub fn leaks(&self) -> u32 {
        self.leaked.load(Ordering::Acquire)
    }

    /// Add a device. Under a hold it is stopped at once, so the hold
    /// covers it too.
    pub fn register(
        &self,
        member: Weak<dyn KernelRings>,
    ) -> Result<(), GateError> {
        let _inside = Inside::enter()?;
        let mut state = self.lock_open()?;
        if state.holds > 0 {
            if let Some(m) = member.upgrade() {
                m.stop().map_err(GateError::Busy)?;
            }
        }
        state.members.push(member);
        Ok(())
    }

    /// Run `f` with every ring worker of this VM gone.
    ///
    /// A refusal to stop runs nothing and leaves every ring running.
    pub fn run<R>(
        &self,
        f: impl FnOnce(&RingsStopped<'_>) -> R,
    ) -> Result<R, GateError> {
        let _inside = Inside::enter()?;
        let mut state = self.lock_open()?;
        let proof = RingsStopped { gate: self };
        if state.holds > 0 {
            return Ok(f(&proof));
        }
        let members = state.live();
        stop_all(&members)?;
        let _restart = Restart(members);
        Ok(f(&proof))
    }

    /// [`Self::run`] for a closure that answers with an I/O result.
    pub fn run_io<T>(
        &self,
        f: impl FnOnce(&RingsStopped<'_>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        self.run(f)?
    }

    /// Run `f` alone, without stopping anything.
    ///
    /// For a member that has to start a worker and stop it again while
    /// a hold says none run: no write-lock ioctl can come between.
    pub fn exclusive<R>(&self, f: impl FnOnce() -> R) -> Result<R, GateError> {
        let _inside = Inside::enter()?;
        let _state = self.lock_open()?;
        Ok(f())
    }

    /// Keep every ring stopped until the answer is dropped.
    pub fn hold(self: &Arc<Self>) -> Result<RingHold, GateError> {
        let _inside = Inside::enter()?;
        let mut state = self.lock_open()?;
        if state.holds == 0 {
            let members = state.live();
            stop_all(&members)?;
        }
        state.holds = state.holds.saturating_add(1);
        Ok(RingHold {
            gate: Arc::clone(self),
        })
    }

    /// Whether a hold keeps the rings stopped.
    pub fn is_held(&self) -> bool {
        self.lock().holds > 0
    }

    /// Whether `rings` was made by this gate.
    pub fn proves(&self, rings: &RingsStopped<'_>) -> bool {
        std::ptr::eq(self, rings.gate)
    }

    fn release(&self) {
        // A release on a thread inside the gate would wait for its own
        // lock. Leaving the rings stopped is the lesser failure.
        let Ok(_inside) = Inside::enter() else {
            debug_assert!(false, "a ring hold was dropped inside the gate");
            return;
        };
        let mut state = self.lock();
        state.holds = state.holds.saturating_sub(1);
        if state.holds == 0 {
            let members = state.live();
            start_all(&members);
        }
    }
}

/// Every ring of one VM stays stopped while this lives.
#[must_use = "the rings start again when this is dropped"]
pub struct RingHold {
    gate: Arc<RingGate>,
}

impl Drop for RingHold {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// The rings a VM pause holds stopped, from `VM_PAUSE` to `VM_RESUME`.
///
/// A ring left running under a paused VM can be started again by a
/// guest kick that was already on its way, and every ioctl the pause is
/// for would race it. So the rings stop before the pause and start
/// only after the resume.
#[derive(Default)]
pub struct PausedRings(Mutex<Option<RingHold>>);

impl PausedRings {
    /// The slot is one `Option`, so a panic cannot leave it half
    /// written.
    fn lock(&self) -> MutexGuard<'_, Option<RingHold>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stop the rings, then run `pause`. A refusal of either leaves
    /// everything running.
    pub fn pause(
        &self,
        gate: &Arc<RingGate>,
        pause: impl FnOnce(&RingsStopped<'_>) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut held = self.lock();
        let hold = gate.hold()?;
        gate.run_io(pause)?;
        *held = Some(hold);
        Ok(())
    }

    /// Run `resume`, then start the rings. A refused resume keeps them
    /// stopped with the VM.
    pub fn resume(
        &self,
        gate: &RingGate,
        resume: impl FnOnce(&RingsStopped<'_>) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut held = self.lock();
        gate.run_io(resume)?;
        drop(held.take());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
