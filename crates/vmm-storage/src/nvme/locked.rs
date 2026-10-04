// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The controller state lock, which raises interrupts only once it is
//! released.
//!
//! Completions are written under the lock and their interrupts noted;
//! releasing the lock raises one MSI per completion queue for all of
//! them. The guest can then run its handler without meeting the lock,
//! and a batch costs one MSI per queue rather than one per completion.

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
    /// Release the lock, raise what is owed, and take the lock again.
    ///
    /// A drain does not count as done while a raise is on its way, so
    /// a reset never clears the pending bits under one.
    fn raise_owed(&mut self) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        let irqs = std::mem::take(&mut guard.irqs);
        if irqs.is_empty() {
            self.guard = Some(guard);
            return;
        }
        let ctrl = self.ctrl;
        ctrl.counts.raising.fetch_add(1, Ordering::AcqRel);
        drop(guard);
        irqs.raise(&ctrl.msix, ctrl.config.msi_policy);
        let guard = ctrl.state.lock().expect("nvme: state lock");
        ctrl.counts.raising.fetch_sub(1, Ordering::AcqRel);
        if guard.drain_waiters > 0 {
            ctrl.drained.notify_all();
        }
        self.guard = Some(guard);
    }

    /// Wait for a completion to wake a drain, for at most `timeout`.
    pub(super) fn wait(&mut self, timeout: Duration) {
        self.raise_owed();
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
        self.raise_owed();
    }
}
