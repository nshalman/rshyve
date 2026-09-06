// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Permission to touch guest memory, and what a device reset takes
//! back.
//!
//! A backing-store call, whether a disk operation or a FUSE call, owns
//! only host memory. Every guest-memory access runs inside a
//! [`Session`], so the wait in [`GuestAccess::drain`] covers the
//! bounded copies a request makes and the used entry that follows
//! them. That is what makes a synchronous reset safe on the vCPU.
//!
//! virtio-blk and virtio-fs share this. Ordering backend work across a
//! reset is a separate problem that only virtio-blk has, because it
//! runs several workers per queue; that lives in
//! `block::access::BackendBarrier`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard};

use crate::pci::intr::IntrSession;

/// Permission to read and write the guest's rings and chains.
pub(crate) struct GuestAccess {
    /// Admission. Cleared before the drain, because a section refused
    /// here never takes the lock. `std::sync::RwLock` promises the
    /// writer no priority, and a guest chooses how much stale work
    /// waits behind a reset, so readers must be turned away by a
    /// check they cannot queue on.
    open: AtomicBool,
    /// One driver session. A request built against an older session
    /// names a ring the driver has since freed.
    epoch: AtomicU64,
    /// The transport session the current epoch runs under, as a raw
    /// [`IntrSession`].
    ///
    /// The device epoch and the transport session are separate
    /// counters, and a completion has to name the transport one to be
    /// admitted. Pairing them here is what makes the name it carries
    /// the session its work was authorised under, rather than
    /// whichever session is running when it arrives.
    ///
    /// Only [`GuestAccess::reopen`] writes it, and only after
    /// [`GuestAccess::drain`], so no reader can be inside while the
    /// pair changes.
    intr: AtomicU64,
    lock: RwLock<()>,
}

/// Permission held across one guest-memory access.
///
/// No code inside a session calls the backing store, parks on the
/// quiesce gate, or raises an interrupt. That is what bounds the wait
/// in [`GuestAccess::drain`].
pub(crate) struct Session<'a> {
    _guard: RwLockReadGuard<'a, ()>,
    epoch: u64,
    intr: IntrSession,
}

impl Session<'_> {
    /// The driver session this permission is good for.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The transport session this permission is good for.
    ///
    /// Work authorised here carries this value and hands it back at
    /// completion, so a reset in between refuses the interrupt instead
    /// of writing it onto the driver that followed.
    pub(crate) fn intr(&self) -> IntrSession {
        self.intr
    }
}

impl GuestAccess {
    pub(crate) fn new() -> Self {
        Self {
            open: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
            intr: AtomicU64::new(IntrSession::INITIAL.raw()),
            lock: RwLock::new(()),
        }
    }

    /// Take permission for `epoch`, or refuse when the device has
    /// moved past it.
    pub(crate) fn enter(&self, epoch: u64) -> Option<Session<'_>> {
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        let guard = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        if !self.open.load(Ordering::Acquire)
            || self.epoch.load(Ordering::Acquire) != epoch
        {
            return None;
        }
        Some(Session {
            _guard: guard,
            epoch,
            intr: self.intr_session(),
        })
    }

    /// Take permission for whichever session is current.
    pub(crate) fn enter_current(&self) -> Option<Session<'_>> {
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        let guard = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        Some(Session {
            _guard: guard,
            epoch: self.epoch.load(Ordering::Acquire),
            intr: self.intr_session(),
        })
    }

    /// Close admission and end the session. Returns the new epoch.
    pub(crate) fn close(&self) -> u64 {
        self.open.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Wait for the sections that were already inside.
    pub(crate) fn drain(&self) {
        drop(self.lock.write().unwrap_or_else(PoisonError::into_inner));
    }

    /// The transport session the current epoch runs under.
    pub(crate) fn intr_session(&self) -> IntrSession {
        IntrSession::from_raw(self.intr.load(Ordering::Acquire))
    }

    /// Admit the next driver, running under `session`.
    ///
    /// Call this after [`Self::drain`]. Admission is shut and no
    /// reader is inside from there until this returns, which is what
    /// makes the epoch and the session one pair to every reader: a
    /// reader holds the read lock across both reads, and this store
    /// cannot happen while it does.
    pub(crate) fn reopen(&self, session: IntrSession) {
        self.intr.store(session.raw(), Ordering::Release);
        self.open.store(true, Ordering::Release);
    }

    /// Raise an interrupt, unless the session that asked for it is
    /// gone.
    ///
    /// The check is one load, and the raise holds nothing a reset
    /// takes. Delivery is a call into the kernel, whose latency is not
    /// ours to bound, and [`Self::close`] runs on the vCPU that wrote
    /// DEVICE_STATUS. So a reset ends the epoch and abandons the
    /// raises already past this point rather than waiting for them.
    ///
    /// This check alone does not keep a raise off the next driver. A
    /// thread parked here while a whole reset runs finds the epoch
    /// already tested and goes on to the transport. What stops it
    /// there is the transport session the raise names, taken from
    /// [`Session::intr`] before the work left its permission: the
    /// transport tests that name in the same step that admits the
    /// delivery, so a raise from a session a reset ended is refused.
    /// This load only saves the call.
    pub(crate) fn deliver(&self, epoch: u64, raise: impl FnOnce()) {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        raise();
    }

    /// Whether sections are being admitted.
    ///
    /// A test polls this to see a reset reach its drain. Polling
    /// [`Self::enter_current`] instead would take the reader lock, and
    /// a caller that already holds one must not take a second while a
    /// writer waits.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Hold the exclusive side the way a reset holds it inside
    /// [`Self::drain`].
    ///
    /// A test uses this to prove that a refused section never queues
    /// on the lock the resetting vCPU waits for.
    #[cfg(test)]
    pub(crate) fn hold_drain(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.lock.write().unwrap_or_else(PoisonError::into_inner)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A refusal that takes longer than this queued on the lock.
    const REFUSAL_BUDGET: Duration = Duration::from_secs(1);

    // The whole point of the admission flag: a section refused while a
    // reset holds the exclusive side must be refused without touching
    // that lock. A reader that queues there is one a guest can use to
    // hold a vCPU, because `std::sync::RwLock` gives the writer no
    // priority.
    #[test]
    fn a_refused_section_never_queues_on_the_drain() {
        let access = Arc::new(GuestAccess::new());
        let stale = access.enter_current().expect("a session is open").epoch();
        access.close();
        // The reset, inside its drain.
        let held = access.hold_drain();

        let refused = Arc::new(AtomicBool::new(false));
        let worker = {
            let access = Arc::clone(&access);
            let refused = Arc::clone(&refused);
            std::thread::spawn(move || {
                if access.enter(stale).is_none()
                    && access.enter_current().is_none()
                {
                    refused.store(true, Ordering::Release);
                }
            })
        };

        let start = Instant::now();
        while !refused.load(Ordering::Acquire)
            && start.elapsed() < REFUSAL_BUDGET
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        let waited = start.elapsed();
        // Let a section that wrongly queued through, so this joins the
        // thread rather than parking on it.
        drop(held);
        worker.join().expect("the section finished");

        assert!(
            refused.load(Ordering::Acquire),
            "a stale request was admitted while a reset held the ring"
        );
        assert!(
            waited < REFUSAL_BUDGET,
            "a refused request queued {waited:?} on the lock the reset needs"
        );
    }

    // An interrupt the closed session asked for must not reach the
    // driver that follows. VirtIO 1.3 sec 2.4.1 forbids queue
    // interaction once a reset is complete.
    #[test]
    fn an_interrupt_from_a_closed_session_is_dropped() {
        let access = GuestAccess::new();
        let stale = access.enter_current().expect("a session is open").epoch();
        let fresh = access.close();
        access.drain();
        access.reopen(IntrSession::INITIAL);

        let fired = AtomicBool::new(false);
        access.deliver(stale, || fired.store(true, Ordering::Release));
        assert!(
            !fired.load(Ordering::Acquire),
            "an interrupt fired for the session the reset ended"
        );

        access.deliver(fresh, || fired.store(true, Ordering::Release));
        assert!(fired.load(Ordering::Acquire), "the next driver gets none");
    }
}
