// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Queue notifications, from the guest and from the backend.
//!
//! A backend that leaves chains in the ring has to drain them later
//! without a guest kick: the guest already kicked for them. That drain
//! takes the same path as a guest write to the notify register, so it
//! obeys the same reset latch and the same lock.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{VirtioDevice, VirtioPciDevice, VirtioPciState};

/// A queue notification a backend raises itself.
pub struct BackendKick {
    kick: Box<dyn Fn(u16) + Send + Sync>,
}

impl BackendKick {
    pub(crate) fn new(kick: impl Fn(u16) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            kick: Box::new(kick),
        })
    }

    /// Drain `queue_idx` as a guest kick would.
    ///
    /// The caller must hold no guest-access permission and no
    /// transport lock: this takes the transport lock, and the notify
    /// path under it takes permission.
    pub fn kick(&self, queue_idx: u16) {
        (self.kick)(queue_idx);
    }
}

/// Where a backend keeps its kick path. Empty until the binary
/// installs it, which it can do only once the transport exists.
#[derive(Default)]
pub struct KickSlot(Mutex<Option<Arc<BackendKick>>>);

impl KickSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, kick: Arc<BackendKick>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(kick);
    }

    /// Kick `queue_idx`. The path is cloned out first, so the slot
    /// lock is not held across the transport lock.
    pub fn kick(&self, queue_idx: u16) {
        let path = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(path) = path {
            path.kick(queue_idx);
        }
    }
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// The kick path a backend of this device drains its rings through.
    ///
    /// Weak for the reason [`Self::backend_intr`] is: the backend lives
    /// inside the transport.
    pub fn backend_kick(self: &Arc<Self>) -> Arc<BackendKick> {
        let weak = Arc::downgrade(self);
        BackendKick::new(move |queue_idx| {
            let Some(dev) = weak.upgrade() else {
                return;
            };
            let Some(vs) = dev.lock_for_write() else {
                return;
            };
            dev.notify(vs, queue_idx);
        })
    }

    /// Run one queue notification under the transport lock, then raise
    /// the interrupt with the lock dropped.
    ///
    /// Takes the guard by value because it releases it: the raise must
    /// not hold the lock, and a refused queue hands it on to
    /// `refuse_queue`.
    pub(super) fn notify(
        &self,
        mut vs: MutexGuard<'_, VirtioPciState>,
        queue_idx: u16,
    ) {
        if !Self::can_notify(&vs, queue_idx) {
            return;
        }
        let did_work =
            self.device
                .notify_queue(queue_idx, &mut vs.queues, &self.physmap);
        if Self::queue_needs_reset(&vs, queue_idx) {
            // The queue logged its own reason when it refused.
            self.refuse_queue(vs, queue_idx as usize, None);
            return;
        }
        if did_work {
            // Sampled under the guard, so a reset that ends this
            // session is always the later of the two.
            let session = self.intr.session();
            drop(vs);
            self.raise_queue_interrupt_in(session, queue_idx);
        }
    }
}
