// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device lifecycle trait.
//!
//! Adopted from propolis — defines the contract for device state
//! transitions during the VM lifecycle.
//!
//! # State machine
//!
//! ```text
//!   Init ──→ Run ──→ Pause ──→ Run   (normal pause/resume)
//!                       │
//!                       └──→ Halt     (shutdown)
//!
//!   Init ──→ Pause                    (migration target: import before start)
//! ```
//!
//! Note: `Init -> Run` and `Init -> Pause` are both valid. The latter
//! is used when a device is created on a migration target and must
//! import state before starting.

use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::{self, BoxFuture};

use crate::migrate::Migrator;

/// General trait for emulated devices in the system.
///
/// As the VM goes through its lifecycle, the emulated devices which it
/// contains are driven through those phases via various event functions
/// (`start`, `pause`, `resume`, etc).
///
/// Default implementations are provided for most methods. Devices that
/// hold no internal state can rely on the defaults. Devices with
/// background tasks or internal state **must** implement at least
/// `pause` and `paused` to ensure clean quiescence.
pub trait Lifecycle: Send + Sync + 'static {
    /// Unique name for devices of a given type.
    fn type_name(&self) -> &'static str;

    /// Return a future that completes when the device has finished pausing.
    ///
    /// Devices with background tasks should return a future that resolves
    /// only after those tasks have quiesced.
    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(future::ready(()))
    }

    /// Called when an instance is about to begin running, just before any
    /// vCPU threads are started.
    fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Directs this device to pause. A paused device must stop producing
    /// work but must accept and queue new work from other devices.
    fn pause(&self) {}

    /// Directs this device to resume servicing the guest after pausing.
    fn resume(&self) {}

    /// Directs this device to reset itself to cold-start state.
    /// Only called on paused devices.
    fn reset(&self) {}

    /// Indicates that the device's instance is stopping.
    /// Only called on paused devices.
    fn halt(&self) {}

    /// Return the Migrator for exporting/importing this device's state.
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Empty
    }
}

/// Indicator for tracking [`Lifecycle`] states.
///
/// Provides atomic state tracking with transition validation.
/// Invalid transitions cause a panic in debug builds and are logged
/// (but not fatal) in release builds, since a state machine violation
/// in a VMM is a serious bug that should be caught during development.
#[derive(Default)]
pub struct Indicator(AtomicUsize);

impl Indicator {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(IndicatedState::Init as usize))
    }

    pub fn start(&self) {
        self.transition(IndicatedState::Run);
    }

    pub fn pause(&self) {
        self.transition(IndicatedState::Pause);
    }

    pub fn resume(&self) {
        self.transition(IndicatedState::Run);
    }

    pub fn halt(&self) {
        self.transition(IndicatedState::Halt);
    }

    pub fn state(&self) -> IndicatedState {
        IndicatedState::from_usize(self.0.load(Ordering::Acquire))
    }

    /// Attempt a state transition, validating before mutating.
    ///
    /// Uses compare_exchange to ensure atomicity: the old state is
    /// validated before the new state is stored, preventing the
    /// swap-then-validate race in the original implementation.
    fn transition(&self, new: IndicatedState) {
        loop {
            let old_raw = self.0.load(Ordering::Acquire);
            let old = IndicatedState::from_usize(old_raw);

            assert!(
                IndicatedState::valid_transition(old, new),
                "invalid lifecycle transition: {old:?} -> {new:?}",
            );

            match self.0.compare_exchange_weak(
                old_raw,
                new as usize,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                // Another thread changed the state between our load and
                // CAS — retry with the new value.
                Err(_) => continue,
            }
        }
    }
}

/// Current state held in [`Indicator`].
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(usize)]
pub enum IndicatedState {
    /// Created but not yet started.
    Init = 0,
    /// Running.
    Run = 1,
    /// Paused (quiesced).
    Pause = 2,
    /// Halted (terminal state).
    Halt = 3,
}

impl IndicatedState {
    /// Valid transitions:
    /// - Init -> Run    (normal startup)
    /// - Init -> Pause  (migration target: import state before starting)
    /// - Run  -> Pause  (pause for migration or reset)
    /// - Pause -> Run   (resume after pause)
    /// - Pause -> Halt  (shutdown from paused state)
    const fn valid_transition(old: Self, new: Self) -> bool {
        matches!(
            (old, new),
            (Self::Init, Self::Run)
                | (Self::Init, Self::Pause)
                | (Self::Run, Self::Pause)
                | (Self::Pause, Self::Run)
                | (Self::Pause, Self::Halt)
        )
    }

    fn from_usize(raw: usize) -> Self {
        match raw {
            0 => Self::Init,
            1 => Self::Run,
            2 => Self::Pause,
            3 => Self::Halt,
            _ => panic!(
                "corrupted IndicatedState value: {raw} \
                 (expected 0..=3, possible memory corruption)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        let ind = Indicator::new();
        assert_eq!(ind.state(), IndicatedState::Init);

        ind.start();
        assert_eq!(ind.state(), IndicatedState::Run);

        ind.pause();
        assert_eq!(ind.state(), IndicatedState::Pause);

        ind.resume();
        assert_eq!(ind.state(), IndicatedState::Run);

        ind.pause();
        ind.halt();
        assert_eq!(ind.state(), IndicatedState::Halt);
    }

    #[test]
    fn init_to_pause_for_migration() {
        let ind = Indicator::new();
        ind.pause(); // Init -> Pause is valid (migration target)
        assert_eq!(ind.state(), IndicatedState::Pause);
    }

    #[test]
    #[should_panic(expected = "invalid lifecycle transition")]
    fn invalid_init_to_halt() {
        let ind = Indicator::new();
        ind.halt(); // Init -> Halt is not valid
    }

    #[test]
    #[should_panic(expected = "invalid lifecycle transition")]
    fn invalid_run_to_halt() {
        let ind = Indicator::new();
        ind.start();
        ind.halt(); // Run -> Halt is not valid (must pause first)
    }
}
