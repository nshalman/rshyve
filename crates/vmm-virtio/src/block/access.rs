// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The order backend work keeps across a device reset.
//!
//! Permission to touch guest memory is `crate::access::GuestAccess`,
//! shared with virtio-fs. Only virtio-blk needs what is here: it runs
//! several workers per queue, so a write admitted under one driver can
//! still overtake the next driver's write to the same sector.

use std::sync::{Condvar, Mutex, PoisonError};

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
