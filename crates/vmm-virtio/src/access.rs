// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Permission to touch guest memory, and what a reset takes back.
//!
//! A backing-store call, whether a disk operation or a FUSE call, owns
//! only host memory. Every guest-memory access runs inside a
//! [`Session`], so the wait in [`GuestAccess::drain`] covers the
//! bounded copies a request makes and the used entry that follows
//! them. That is what makes a synchronous reset safe on the vCPU.
//!
//! Permission is held per virtqueue. A driver may program one ring
//! again without resetting the device, and that ring alone is the one
//! it gave up: work in flight on every other queue stays valid and
//! must still reach its used ring.
//!
//! virtio-blk, virtio-fs and virtio-vsock share this. Ordering backend
//! work across a retirement is a separate problem that only virtio-blk
//! has, because it runs several workers per queue; that lives in
//! `block::access::BackendBarrier`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard};

use crate::pci::intr::IntrSession;

/// The ring a piece of deferred work belongs to.
///
/// Work authorised on one vCPU completes on another thread, and the
/// driver can reprogram that ring in between. Carrying the name of the
/// ring and the generation it was authorised under is what lets the
/// completion be refused rather than written into pages the guest has
/// since reused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RingTag {
    queue_idx: u16,
    generation: u64,
}

impl RingTag {
    pub(crate) fn queue_idx(self) -> u16 {
        self.queue_idx
    }
}

/// Permission to read and write the guest's rings and chains.
pub(crate) struct GuestAccess {
    /// Admission. Cleared before the drain, because a section refused
    /// here never takes the lock. `std::sync::RwLock` promises the
    /// writer no priority, and a guest chooses how much stale work
    /// waits behind a retirement, so readers must be turned away by a
    /// check they cannot queue on.
    open: AtomicBool,
    /// One driver generation per virtqueue. Work naming a generation
    /// the queue has left points at a ring the driver has since freed.
    generations: Box<[AtomicU64]>,
    /// The transport session the current generations run under, as a
    /// raw [`IntrSession`].
    ///
    /// A generation and the transport session are separate counters,
    /// and a completion has to name the transport one to be admitted.
    /// Pairing them here is what makes the name work carries the
    /// session it was authorised under, rather than whichever session
    /// is running when it arrives.
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
    tag: RingTag,
    intr: IntrSession,
}

impl Session<'_> {
    /// The ring and generation this permission is good for.
    pub(crate) fn tag(&self) -> RingTag {
        self.tag
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
    /// Permission for a device with `num_queues` virtqueues.
    pub(crate) fn new(num_queues: usize) -> Self {
        Self {
            open: AtomicBool::new(true),
            generations: (0..num_queues).map(|_| AtomicU64::new(0)).collect(),
            intr: AtomicU64::new(IntrSession::INITIAL.raw()),
            lock: RwLock::new(()),
        }
    }

    /// Take permission for `tag`, or refuse when the queue it names
    /// has moved past it.
    ///
    /// A queue index the device was not sized for names no ring, so it
    /// is refused rather than tested against another queue.
    pub(crate) fn enter(&self, tag: RingTag) -> Option<Session<'_>> {
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        let slot = self.generations.get(usize::from(tag.queue_idx))?;
        let guard = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        if !self.open.load(Ordering::Acquire)
            || slot.load(Ordering::Acquire) != tag.generation
        {
            return None;
        }
        Some(Session {
            _guard: guard,
            tag,
            intr: self.intr_session(),
        })
    }

    /// Take permission for whichever generation `queue_idx` is on.
    pub(crate) fn enter_current(&self, queue_idx: u16) -> Option<Session<'_>> {
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        let slot = self.generations.get(usize::from(queue_idx))?;
        let guard = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        Some(Session {
            _guard: guard,
            tag: RingTag {
                queue_idx,
                generation: slot.load(Ordering::Acquire),
            },
            intr: self.intr_session(),
        })
    }

    /// Whether `tag` still names the generation its queue is on.
    ///
    /// Takes no permission, so a caller that may then wait on
    /// something else can ask. A caller about to touch guest memory
    /// must use [`Self::enter`] instead: this answer is stale the
    /// moment it is read.
    pub(crate) fn is_current(&self, tag: RingTag) -> bool {
        self.generations
            .get(usize::from(tag.queue_idx))
            .is_some_and(|slot| slot.load(Ordering::Acquire) == tag.generation)
    }

    /// Close admission and end one queue's generation. Returns the new
    /// generation.
    ///
    /// Every other queue keeps the generation it is on, so work in
    /// flight there still completes.
    pub(crate) fn close_queue(&self, queue_idx: u16) -> u64 {
        self.open.store(false, Ordering::Release);
        match self.generations.get(usize::from(queue_idx)) {
            Some(slot) => slot.fetch_add(1, Ordering::AcqRel) + 1,
            // Nothing was ever admitted for a queue the device was not
            // sized for, so there is nothing to retire.
            None => 0,
        }
    }

    /// Close admission and end every queue's generation, as a device
    /// reset does.
    pub(crate) fn close_all(&self) {
        self.open.store(false, Ordering::Release);
        for slot in self.generations.iter() {
            slot.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Wait for the sections that were already inside.
    pub(crate) fn drain(&self) {
        drop(self.lock.write().unwrap_or_else(PoisonError::into_inner));
    }

    /// The transport session the current generations run under.
    pub(crate) fn intr_session(&self) -> IntrSession {
        IntrSession::from_raw(self.intr.load(Ordering::Acquire))
    }

    /// Admit the next driver, running under `session`.
    ///
    /// Call this after [`Self::drain`]. Admission is shut and no
    /// reader is inside from there until this returns, which is what
    /// makes the generation and the session one pair to every reader:
    /// a reader holds the read lock across both reads, and this store
    /// cannot happen while it does.
    pub(crate) fn reopen(&self, session: IntrSession) {
        self.intr.store(session.raw(), Ordering::Release);
        self.open.store(true, Ordering::Release);
    }

    /// Raise an interrupt, unless the generation that asked for it is
    /// gone.
    ///
    /// The check is one load, and the raise holds nothing a reset
    /// takes. Delivery is a call into the kernel, whose latency is not
    /// ours to bound, and a reset runs on the vCPU that wrote
    /// DEVICE_STATUS. So a reset ends the generation and abandons the
    /// raises already past this point rather than waiting for them.
    ///
    /// This check alone does not keep a raise off the next driver. A
    /// thread parked here while a whole reset runs finds the
    /// generation already tested and goes on to the transport. What
    /// stops it there is the transport session the raise names, taken
    /// from [`Session::intr`] before the work left its permission: the
    /// transport tests that name in the same step that admits the
    /// delivery, so a raise from a session a reset ended is refused.
    /// This load only saves the call.
    pub(crate) fn deliver(&self, tag: RingTag, raise: impl FnOnce()) {
        if !self.is_current(tag) {
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

    /// Whether a section is open now: a reset's drain would wait.
    #[cfg(test)]
    pub(crate) fn in_section(&self) -> bool {
        matches!(
            self.lock.try_write(),
            Err(std::sync::TryLockError::WouldBlock)
        )
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
        let access = Arc::new(GuestAccess::new(1));
        let stale = access.enter_current(0).expect("a session is open").tag();
        access.close_all();
        // The reset, inside its drain.
        let held = access.hold_drain();

        let refused = Arc::new(AtomicBool::new(false));
        let worker = {
            let access = Arc::clone(&access);
            let refused = Arc::clone(&refused);
            std::thread::spawn(move || {
                if access.enter(stale).is_none()
                    && access.enter_current(0).is_none()
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

    // An interrupt the closed generation asked for must not reach the
    // driver that follows. VirtIO 1.3 sec 2.4.1 forbids queue
    // interaction once a reset is complete.
    #[test]
    fn an_interrupt_from_a_closed_session_is_dropped() {
        let access = GuestAccess::new(1);
        let stale = access.enter_current(0).expect("a session is open").tag();
        access.close_all();
        access.drain();
        access.reopen(IntrSession::INITIAL);
        let fresh = access.enter_current(0).expect("the next driver").tag();

        let fired = AtomicBool::new(false);
        access.deliver(stale, || fired.store(true, Ordering::Release));
        assert!(
            !fired.load(Ordering::Acquire),
            "an interrupt fired for the generation the reset ended"
        );

        access.deliver(fresh, || fired.store(true, Ordering::Release));
        assert!(fired.load(Ordering::Acquire), "the next driver gets none");
    }

    // Reprogramming one ring must leave every other queue's work
    // valid. A device-wide retirement here strands whatever those
    // queues had in flight.
    #[test]
    fn retiring_one_queue_leaves_the_others_admitted() {
        let access = GuestAccess::new(2);
        let other = access.enter_current(1).expect("queue 1 is open").tag();
        let stale = access.enter_current(0).expect("queue 0 is open").tag();

        access.close_queue(0);
        access.drain();
        access.reopen(IntrSession::INITIAL);

        assert!(
            access.enter(other).is_some(),
            "queue 1 lost its permission when queue 0 was reprogrammed"
        );
        assert!(
            access.enter(stale).is_none(),
            "queue 0 kept the generation the driver gave up"
        );
    }

    // A queue index the device was never sized for names no ring.
    #[test]
    fn a_queue_past_the_device_is_refused() {
        let access = GuestAccess::new(1);
        let tag = RingTag {
            queue_idx: 4,
            generation: 0,
        };
        assert!(access.enter(tag).is_none());
        assert!(access.enter_current(4).is_none());
        assert!(!access.is_current(tag));
    }
}
