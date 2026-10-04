// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The backing-store work a device has taken from its rings and not
//! yet finished.
//!
//! A pause and a migration wait for every request the device took, so
//! that amount is the quiesce bound. The guest picks how much it
//! posts, so the device takes no more than this budget and leaves the
//! rest in the ring, where a migration carries it as ring state.

use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::Duration;

use super::{bits, discard};

/// Bytes of backing-store work one device may have in flight.
///
/// Two of the largest requests a Linux driver can build under the
/// advertised limits (126 x 128 KiB) fit with room to spare, and a
/// queue depth of 1 MiB requests far past what a zvol needs to stay
/// busy. At 100 MB/s, which is a slow pool, the backlog a pause waits
/// for is under a second.
pub(super) const INFLIGHT_BYTES: u64 = 64 << 20;

/// How long a pause lets a request past the budget run on before it
/// stops it between chunks.
///
/// Under the 5 s operator pause budget, with 2 s left for the other
/// devices and the budgeted backlog. A firmware boot file read, the
/// usual such request, ends well inside it. One that does not fails
/// the pause, which can be tried again after a resume.
pub(super) const OVERSIZED_PAUSE_GRACE: Duration = Duration::from_secs(3);

/// Requests one device may have in flight, whatever they cost.
///
/// One full ring of the size the transport gives virtio-blk (128), so
/// a single-queue guest is never held below its own queue depth. With
/// at most 16 workers that is 8 queued per worker, enough to keep each
/// one busy. It also bounds a guest that posts one head many times
/// before the device answers it: each copy holds a slot until it is
/// done, and the guest gets the copies it asked for.
pub(super) const MAX_REQUESTS: usize = 128;

/// The least a request costs. FLUSH and DISCARD name no data bytes,
/// but each one is still a call to the backing store.
pub(super) const MIN_COST: u64 = 4096;

/// One bit per queue in a `u64` mask.
pub(super) const MAX_QUEUES: u16 = u64::BITS as u16;

/// Taken from a queue's ring and not yet finished.
struct State {
    bytes: u64,
    requests: usize,
    /// Queues that gave a chain back for want of budget, one bit each.
    starved: u64,
    /// What each starved queue was refused for.
    refused: [u64; MAX_QUEUES as usize],
    /// Starved queues a completion has freed budget for and that no
    /// worker has kicked yet.
    kick: u64,
    /// The queue that went first in the last kick.
    last_first: Option<u16>,
    #[cfg(test)]
    peak_bytes: u64,
    #[cfg(test)]
    peak_requests: usize,
}

pub(super) struct Budget {
    limit: u64,
    state: Mutex<State>,
}

impl Budget {
    pub(super) fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(State {
                bytes: 0,
                requests: 0,
                starved: 0,
                refused: [0; MAX_QUEUES as usize],
                kick: 0,
                last_first: None,
                #[cfg(test)]
                peak_bytes: 0,
                #[cfg(test)]
                peak_requests: 0,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a request of `cost` may join what is in flight.
    ///
    /// A request larger than the whole budget runs when nothing else is
    /// in flight. Firmware reads its boot file in one such request, and
    /// it would never run otherwise.
    fn fits(&self, s: &State, cost: u64) -> bool {
        s.requests == 0
            || (s.requests < MAX_REQUESTS
                && s.bytes.saturating_add(cost) <= self.limit)
    }

    /// Charge `cost` for a request on `queue`, or mark the queue
    /// starved and return `None`.
    ///
    /// The mark and the refusal share one lock with the release, so a
    /// completion cannot slip between them and leave the queue waiting
    /// for a kick that never comes. A refusal needs work in flight, so
    /// a release always follows a mark.
    pub(super) fn charge(
        self: &Arc<Self>,
        queue: u16,
        cost: u64,
    ) -> Option<Charge> {
        let cost = cost.max(MIN_COST);
        let mut s = self.state();
        if !self.fits(&s, cost) {
            s.starved |= queue_bit(queue);
            s.refused[usize::from(queue)] = cost;
            return None;
        }
        s.bytes = s.bytes.saturating_add(cost);
        s.requests += 1;
        #[cfg(test)]
        {
            s.peak_bytes = s.peak_bytes.max(s.bytes);
            s.peak_requests = s.peak_requests.max(s.requests);
        }
        Some(Charge {
            budget: Some(Arc::clone(self)),
            cost,
            alone: cost > self.limit,
        })
    }

    /// Give back `cost`. A starved queue becomes due for a kick once
    /// the head it was refused for fits, and half the budget is free
    /// or nothing is in flight.
    ///
    /// Waiting for half keeps a saturated device from draining its
    /// rings once per completion, and waiting for the refused head to
    /// fit keeps a head larger than the budget from being drained and
    /// refused on every completion. Nothing in flight always releases
    /// them: no later completion would.
    fn release(&self, cost: u64) {
        let mut s = self.state();
        s.bytes = s.bytes.saturating_sub(cost);
        s.requests = s.requests.saturating_sub(1);
        let low = s.requests == 0
            || (s.bytes <= self.limit / 2 && s.requests <= MAX_REQUESTS / 2);
        if !low {
            return;
        }
        let mut due = 0u64;
        for queue in queues(s.starved) {
            if self.fits(&s, s.refused[usize::from(queue)]) {
                due |= queue_bit(queue);
            }
        }
        s.starved &= !due;
        s.kick |= due;
    }

    /// Queues due for a kick, each returned once.
    ///
    /// The first queue gets the freed budget first, so first place goes
    /// round the due queues. Going round queue numbers would favor the
    /// queue after a long gap.
    pub(super) fn take_kicks(&self) -> Vec<u16> {
        let mut s = self.state();
        let mask = std::mem::take(&mut s.kick);
        let after = s.last_first.map_or(0, |q| {
            mask & u64::MAX.checked_shl(u32::from(q) + 1).unwrap_or(0)
        });
        let order: Vec<u16> =
            queues(after).chain(queues(mask & !after)).collect();
        if let Some(&first) = order.first() {
            s.last_first = Some(first);
        }
        order
    }

    /// Whether nothing is in flight. Never blocks: a contended lock
    /// reads as busy, and the caller polls again.
    pub(super) fn is_idle(&self) -> bool {
        match self.state.try_lock() {
            Ok(s) => s.requests == 0,
            Err(TryLockError::Poisoned(p)) => p.into_inner().requests == 0,
            Err(TryLockError::WouldBlock) => false,
        }
    }

    /// (bytes, requests) in flight now.
    #[cfg(test)]
    pub(super) fn in_flight(&self) -> (u64, usize) {
        let s = self.state();
        (s.bytes, s.requests)
    }

    /// The most (bytes, requests) ever in flight at once.
    #[cfg(test)]
    pub(super) fn peak(&self) -> (u64, usize) {
        let s = self.state();
        (s.peak_bytes, s.peak_requests)
    }
}

fn queue_bit(queue: u16) -> u64 {
    // The device refuses more queues than the mask holds.
    1u64 << queue
}

/// Bits of `mask`, as queue indexes.
fn queues(mut mask: u64) -> impl Iterator<Item = u16> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let q = mask.trailing_zeros() as u16;
        mask &= mask - 1;
        Some(q)
    })
}

/// Budget one request holds until it is dropped.
///
/// Every path a request can take ends in a drop: done, refused, from a
/// closed session, or lost with its worker. So the budget comes back
/// on every one of them.
#[derive(Default)]
pub(super) struct Charge {
    budget: Option<Arc<Budget>>,
    cost: u64,
    alone: bool,
}

impl Charge {
    /// Whether this request costs more than the whole budget, so the
    /// budget does not bound how long it runs.
    pub(super) fn is_alone(&self) -> bool {
        self.alone
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.take() {
            budget.release(self.cost);
        }
    }
}

/// Bytes of backing-store work a checked request can cause.
///
/// `span` is every data byte the chain names. A WRITE_ZEROES payload
/// is 16 bytes per range and each range can zero up to its maximum,
/// so the cost is that maximum per range the payload has room for. The
/// worker refuses a payload with more ranges than that. DISCARD and
/// FLUSH move no data. [`Budget::charge`] raises each cost to at least
/// [`MIN_COST`].
pub(super) fn cost(rtype: u32, span: usize) -> u64 {
    match rtype {
        bits::VIRTIO_BLK_T_IN | bits::VIRTIO_BLK_T_OUT => span as u64,
        bits::VIRTIO_BLK_T_WRITE_ZEROES => {
            let ranges =
                (span / discard::SEG_SIZE).min(discard::MAX_SEG as usize);
            ranges as u64
                * u64::from(discard::MAX_WRITE_ZEROES_SECTORS)
                * bits::SECTOR_SIZE
        }
        _ => 0,
    }
}
