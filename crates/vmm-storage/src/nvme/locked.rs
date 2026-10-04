// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The controller state lock. Completions are written under it and
//! their interrupts noted; releasing it raises one MSI per completion
//! queue for all of them, so the guest's handler never meets the lock.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::Ordering;
use std::sync::MutexGuard;
use std::time::Duration;

use super::queues::NvmeState;
use super::NvmeController;

pub(super) struct Locked<'a> {
    ctrl: &'a NvmeController,
    guard: Option<MutexGuard<'a, NvmeState>>,
}

impl NvmeController {
    pub(super) fn lock(&self) -> Locked<'_> {
        Locked {
            ctrl: self,
            guard: Some(self.state.lock().expect("nvme: state lock")),
        }
    }
}

impl Locked<'_> {
    /// Returns whether the lock was released to raise MSIs.
    ///
    /// `raising` goes up before `in_flight` drops, both under the lock,
    /// so a lock-free reader never sees the device quiet while a CQE or
    /// its MSI is still pending, and a reset never clears pending bits
    /// under a raise.
    fn raise_owed(&mut self) -> bool {
        let Some(mut guard) = self.guard.take() else {
            return false;
        };
        let counts = &self.ctrl.counts;
        let retired = std::mem::take(&mut guard.retiring);
        let irqs = std::mem::take(&mut guard.irqs);
        if irqs.is_empty() {
            counts.in_flight.fetch_sub(retired, Ordering::AcqRel);
            self.guard = Some(guard);
            return false;
        }
        let ctrl = self.ctrl;
        counts.raising.fetch_add(1, Ordering::AcqRel);
        counts.in_flight.fetch_sub(retired, Ordering::AcqRel);
        drop(guard);
        irqs.raise(&ctrl.msix, ctrl.config.msi_policy);
        let guard = ctrl.state.lock().expect("nvme: state lock");
        counts.raising.fetch_sub(1, Ordering::AcqRel);
        if guard.drain_waiters > 0 {
            ctrl.drained.notify_all();
        }
        self.guard = Some(guard);
        true
    }

    /// Returns at once when raising what was owed released the lock: a
    /// completion may have come in that gap, so the caller looks again
    /// before it sleeps.
    pub(super) fn wait(&mut self, timeout: Duration) {
        if self.raise_owed() {
            return;
        }
        let Some(guard) = self.guard.take() else {
            return;
        };
        let (guard, _) = self
            .ctrl
            .drained
            .wait_timeout(guard, timeout)
            .expect("nvme: state lock");
        self.guard = Some(guard);
    }
}

impl Deref for Locked<'_> {
    type Target = NvmeState;

    fn deref(&self) -> &NvmeState {
        self.guard.as_ref().expect("nvme: the state lock is held")
    }
}

impl DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut NvmeState {
        self.guard.as_mut().expect("nvme: the state lock is held")
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // Unwinding poisons the lock, and taking it again would turn
        // one panic into an abort.
        if !std::thread::panicking() {
            self.raise_owed();
        }
    }
}
