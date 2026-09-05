// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Permission to touch guest memory, and the order backend work keeps
//! across a device reset.
//!
//! A backing-store call owns only host memory. Every guest-memory
//! access runs inside a [`Session`], so the wait in
//! [`GuestAccess::drain`] covers memory copies and ring writes only.
//! That is what makes a synchronous reset safe on the vCPU.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard};

/// Permission to read and write the guest's rings and chains.
pub(super) struct GuestAccess {
    /// Admission. Cleared before the drain, because a section refused
    /// here never takes the lock. `std::sync::RwLock` promises the
    /// writer no priority, and a guest chooses how much stale work
    /// waits behind a reset, so readers must be turned away by a
    /// check they cannot queue on.
    open: AtomicBool,
    /// One driver session. A request built against an older session
    /// names a ring the driver has since freed.
    epoch: AtomicU64,
    lock: RwLock<()>,
    /// Orders interrupt delivery against the reset that ends the
    /// session the interrupt belongs to. Shared, so queues still
    /// deliver in parallel.
    intr: RwLock<()>,
}

/// Permission held across one guest-memory access.
///
/// No code inside a session calls the backing store, parks on the
/// quiesce gate, or raises an interrupt. That is what bounds the wait
/// in [`GuestAccess::drain`].
pub(super) struct Session<'a> {
    _guard: RwLockReadGuard<'a, ()>,
    epoch: u64,
}

impl Session<'_> {
    /// The driver session this permission is good for.
    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl GuestAccess {
    pub(super) fn new() -> Self {
        Self {
            open: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
            lock: RwLock::new(()),
            intr: RwLock::new(()),
        }
    }

    /// Take permission for `epoch`, or refuse when the device has
    /// moved past it.
    pub(super) fn enter(&self, epoch: u64) -> Option<Session<'_>> {
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
        })
    }

    /// Take permission for whichever session is current.
    pub(super) fn enter_current(&self) -> Option<Session<'_>> {
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
        })
    }

    /// Close admission and end the session. Returns the new epoch.
    pub(super) fn close(&self) -> u64 {
        self.open.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Wait for the sections that were already inside.
    pub(super) fn drain(&self) {
        drop(self.lock.write().unwrap_or_else(PoisonError::into_inner));
    }

    /// Wait for the interrupts the closed session had already decided
    /// to raise. VirtIO 1.3 sec 2.4.1 forbids queue interaction once a
    /// reset is complete.
    pub(super) fn settle_interrupts(&self) {
        drop(self.intr.write().unwrap_or_else(PoisonError::into_inner));
    }

    /// Admit the next driver.
    pub(super) fn reopen(&self) {
        self.open.store(true, Ordering::Release);
    }

    /// Raise an interrupt, unless the session that asked for it is
    /// gone.
    ///
    /// The gate makes the session check and the delivery one step
    /// against [`Self::settle_interrupts`], so the raise reaches the
    /// driver it was meant for or does not happen.
    pub(super) fn deliver(&self, epoch: u64, raise: impl FnOnce()) {
        let _gate = self.intr.read().unwrap_or_else(PoisonError::into_inner);
        if self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        raise();
    }
}

/// Keeps backend work in generation order across a reset.
///
/// A write already inside `pwrite` when a reset lands still reaches
/// the disk. It reads a host snapshot, so it cannot touch guest
/// memory, but it can still overtake a write the next driver sends to
/// the same sector. New work waits behind those.
///
/// This never gates the reset itself. A disk that never answers must
/// be able to stop I/O progress while the reset still returns.
pub(super) struct BackendBarrier {
    state: Mutex<BarrierState>,
    settled: Condvar,
}

struct BarrierState {
    /// The session new backend work must name.
    generation: u64,
    /// Running operations of `generation` that change the disk.
    live_writes: usize,
    /// Running operations that change the disk and were admitted
    /// under an earlier session. New work waits for these.
    stale_writes: usize,
}

/// One admitted backend operation, counted until it is dropped.
pub(super) struct Ticket<'a> {
    barrier: &'a BackendBarrier,
    generation: u64,
    writes: bool,
}

impl BackendBarrier {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(BarrierState {
                generation: 0,
                live_writes: 0,
                stale_writes: 0,
            }),
            settled: Condvar::new(),
        }
    }

    /// Admit one backend operation for `generation`.
    ///
    /// `writes` marks an operation that changes the disk, which is the
    /// only kind a later generation must wait for. Returns `None` for
    /// work the reset cancelled. The check and the count are one step
    /// under the lock the reset takes, so no operation is admitted
    /// under a session that has ended.
    pub(super) fn admit(
        &self,
        generation: u64,
        writes: bool,
    ) -> Option<Ticket<'_>> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if st.generation != generation {
                return None;
            }
            if st.stale_writes == 0 {
                break;
            }
            st = self
                .settled
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if writes {
            st.live_writes += 1;
        }
        Some(Ticket {
            barrier: self,
            generation,
            writes,
        })
    }

    /// End `generation`. Operations still running become the set the
    /// next generation waits for.
    pub(super) fn retire(&self, generation: u64) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.generation = generation;
        st.stale_writes += st.live_writes;
        st.live_writes = 0;
        // Wake the waiters that must now give up: their generation has
        // ended even if nothing stale is left.
        self.settled.notify_all();
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.writes {
            return;
        }
        let mut st = self
            .barrier
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if st.generation == self.generation {
            st.live_writes -= 1;
        } else {
            st.stale_writes -= 1;
            if st.stale_writes == 0 {
                self.barrier.settled.notify_all();
            }
        }
    }
}
