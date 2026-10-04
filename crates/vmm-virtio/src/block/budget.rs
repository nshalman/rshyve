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

use std::sync::{Arc, Mutex, PoisonError};

use super::{bits, discard};

/// Bytes of backing-store work one device may have in flight.
///
/// Two of the largest requests a Linux driver can build under the
/// advertised limits (126 x 128 KiB) fit with room to spare, and a
/// queue depth of 1 MiB requests far past what a zvol needs to stay
/// busy. At 100 MB/s, which is a slow pool, the backlog a pause waits
/// for is under a second.
pub(super) const INFLIGHT_BYTES: u64 = 64 << 20;

/// Taken from a queue's ring and not yet finished.
#[derive(Default)]
struct State {
    bytes: u64,
    requests: usize,
    /// Queues that gave a chain back for want of budget, one bit each.
    starved: u64,
    /// Starved queues a completion has freed budget for and that no
    /// worker has kicked yet.
    kick: u64,
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
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Charge `cost` for a request on `queue`, or mark the queue
    /// starved and return `None`.
    ///
    /// The mark and the refusal share one lock with the release, so a
    /// completion cannot slip between them and leave the queue waiting
    /// for a kick that never comes. A refusal needs work in flight, so
    /// a release always follows a mark.
    ///
    /// A request larger than the whole budget runs when nothing else is
    /// in flight. Firmware reads its boot file in one such request, and
    /// it would never run otherwise.
    pub(super) fn charge(
        self: &Arc<Self>,
        queue: u16,
        cost: u64,
    ) -> Option<Charge> {
        let mut s = self.state();
        if s.requests != 0 && s.bytes.saturating_add(cost) > self.limit {
            s.starved |= queue_bit(queue);
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
        })
    }

    /// Give back `cost`. Starved queues become due for a kick once
    /// half the budget is free, or once nothing is in flight.
    ///
    /// Waiting for half keeps a saturated device from draining its
    /// rings once per completion. Nothing in flight always releases
    /// them: no later completion would.
    fn release(&self, cost: u64) {
        let mut s = self.state();
        s.bytes = s.bytes.saturating_sub(cost);
        s.requests = s.requests.saturating_sub(1);
        if s.requests == 0 || s.bytes <= self.limit / 2 {
            s.kick |= std::mem::take(&mut s.starved);
        }
    }

    /// Queues due for a kick, one bit each. Each is returned once.
    pub(super) fn take_kicks(&self) -> u64 {
        std::mem::take(&mut self.state().kick)
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
pub(super) fn queues(mut mask: u64) -> impl Iterator<Item = u16> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let q = mask.trailing_zeros() as u16;
        mask &= mask - 1;
        Some(q)
    })
}

/// Most queues a budget can track.
pub(super) const MAX_QUEUES: u16 = u64::BITS as u16;

/// Budget one request holds until it is dropped.
///
/// Every path a request can take ends in a drop: done, refused, from a
/// closed session, or lost with its worker. So the budget comes back
/// on every one of them.
#[derive(Default)]
pub(super) struct Charge {
    budget: Option<Arc<Budget>>,
    cost: u64,
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
/// FLUSH move no data.
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
