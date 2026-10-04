// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Queue state and completion posting.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::bits::*;
use super::cq::{Held, Irqs};
use super::inflight::SqSlots;

/// One queue as the driver created it.
///
/// A queue slot is reused as soon as the driver deletes and recreates
/// it, so work in flight names the instance it was submitted against.
/// A completion whose instance is gone belongs to a queue the driver
/// has already freed.
pub(super) type QueueInstance = u64;

pub(super) struct SubQueue {
    /// Guest physical address of the queue base.
    pub(super) base: u64,
    /// Number of entries (1-based, max entries).
    pub(super) size: u16,
    /// Head pointer (consumer side, advanced by controller).
    pub(super) head: u16,
    /// Tail pointer (updated by guest via doorbell write).
    pub(super) tail: u16,
    /// Associated completion queue ID.
    pub(super) cq_id: u16,
    pub(super) instance: QueueInstance,
}

pub(super) struct CompQueue {
    /// Guest physical address of the queue base.
    pub(super) base: u64,
    /// Number of entries (1-based, max entries).
    pub(super) size: u16,
    /// Head pointer (updated by guest via doorbell write).
    pub(super) head: u16,
    /// Tail pointer (producer side, advanced by controller).
    pub(super) tail: u16,
    /// Phase tag — toggles on each wrap of the CQ.
    pub(super) phase: bool,
    /// MSI-X interrupt vector for this CQ.
    pub(super) iv: u16,
    /// Whether the driver asked for an interrupt on this CQ.
    pub(super) ien: bool,
    pub(super) instance: QueueInstance,
    /// Completions waiting for the guest to free a slot, oldest first.
    pub(super) held: VecDeque<Held>,
}

/// The instance each I/O queue slot holds now, readable without the
/// state lock. Zero is no queue.
#[derive(Default)]
pub(super) struct QueueEpochs {
    sq: [AtomicU64; MAX_IO_QUEUES],
    cq: [AtomicU64; MAX_IO_QUEUES],
}

impl QueueEpochs {
    pub(super) fn is_current(
        &self,
        sq: usize,
        sq_instance: QueueInstance,
        cq: usize,
        cq_instance: QueueInstance,
    ) -> bool {
        let live = |slots: &[AtomicU64], idx: usize, want: QueueInstance| {
            slots
                .get(idx)
                .is_some_and(|e| e.load(Ordering::Acquire) == want)
        };
        live(&self.sq, sq, sq_instance) && live(&self.cq, cq, cq_instance)
    }
}

/// Who may put commands on the backend.
///
/// Checked and changed only under the state lock, so a drain that
/// counts nothing in flight cannot be overtaken by an admission.
#[derive(Default)]
pub(super) struct Admission {
    /// The control plane paused the device.
    pub(super) paused: bool,
    /// Resets, shutdowns, flushes and exports waiting for a drain.
    pub(super) draining: u32,
    /// The backend is stopped for good.
    pub(super) halted: bool,
}

impl Admission {
    pub(super) fn is_open(&self) -> bool {
        !self.paused && self.draining == 0 && !self.halted
    }
}

pub(super) fn io_queue_index(qid: u16) -> Option<usize> {
    let idx = usize::from(qid.checked_sub(1)?);
    (idx < MAX_IO_QUEUES).then_some(idx)
}

// ---------------------------------------------------------------------------
// NVMe controller state (under lock)
// ---------------------------------------------------------------------------

pub(super) struct NvmeState {
    // Controller registers
    pub(super) cap: u64,
    pub(super) cc: u32,
    pub(super) csts: u32,
    pub(super) aqa: u32,
    pub(super) asq_base: u64,
    pub(super) acq_base: u64,

    // Queues
    pub(super) admin_sq: Option<SubQueue>,
    pub(super) admin_cq: Option<CompQueue>,
    pub(super) io_sqs: [Option<SubQueue>; MAX_IO_QUEUES],
    pub(super) io_cqs: [Option<CompQueue>; MAX_IO_QUEUES],

    // Pre-built Identify data
    pub(super) ctrl_ident: [u8; IDENT_CTRL_SIZE],
    pub(super) ns_ident: [u8; IDENT_NS_SIZE],

    // Configuration
    pub(super) num_io_queues: u16,
    /// The I/O queues the backend serves, the most Set Features grants.
    pub(super) max_io_queues: u16,
    /// Whether Identify reports a volatile write cache.
    pub(super) volatile_write_cache: bool,

    /// Source of [`QueueInstance`] values. Never reset, so an instance
    /// is unique for the life of the controller.
    pub(super) instances: QueueInstance,

    /// Whether a fence deadline passed with a worker still able to
    /// reach guest memory. Sticky: see [`Self::fail_fence`].
    pub(super) fence_failed: bool,

    pub(super) admission: Admission,
    /// Per I/O SQ, the commands it has admitted.
    pub(super) inflight: [SqSlots; MAX_IO_QUEUES],
    /// Per I/O CQ, the admitted commands that will complete into it.
    pub(super) cq_inflight: [u32; MAX_IO_QUEUES],
    /// I/O SQs that stopped at their depth, one bit each.
    pub(super) throttled: u16,
    /// Source of [`super::backend::RequestId`] serials.
    pub(super) serial: u64,
    pub(super) epochs: Arc<QueueEpochs>,
    /// Threads waiting in a drain, so a completion knows to wake one.
    pub(super) drain_waiters: u32,
    /// Interrupts owed for completions written since the lock was taken.
    pub(super) irqs: Irqs,
    /// Commands finished since the lock was taken, still counted in
    /// the lock-free in-flight count.
    pub(super) retiring: usize,
}

impl NvmeState {
    /// Take the queues away from the driver and from anything in
    /// flight against them.
    pub(super) fn retire_queues(&mut self) {
        self.admin_sq = None;
        self.admin_cq = None;
        for idx in 0..MAX_IO_QUEUES {
            self.set_sq(idx, None);
            self.set_cq(idx, None);
        }
        self.throttled = 0;
    }

    /// Put `sq` in I/O SQ slot `idx`. Every change goes through here so
    /// a request in a backend can tell its queue is gone, and so the
    /// completions the old queue had waiting go with it.
    pub(super) fn set_sq(&mut self, idx: usize, sq: Option<SubQueue>) {
        let Some(slot) = self.io_sqs.get_mut(idx) else {
            return;
        };
        let instance = sq.as_ref().map_or(0, |q| q.instance);
        let old = std::mem::replace(slot, sq);
        self.epochs.sq[idx].store(instance, Ordering::Release);

        let Some(old) = old else {
            return;
        };
        let sqid = idx as u16 + 1;
        let Some(cq) = io_queue_index(old.cq_id)
            .and_then(|cq_idx| self.io_cqs[cq_idx].as_mut())
        else {
            return;
        };
        let (gone, kept) = std::mem::take(&mut cq.held)
            .into_iter()
            .partition(|held| held.cqe.sqid == sqid);
        cq.held = kept;
        self.release_held(gone);
    }

    /// The same for I/O CQ slot `idx`.
    pub(super) fn set_cq(&mut self, idx: usize, cq: Option<CompQueue>) {
        let Some(slot) = self.io_cqs.get_mut(idx) else {
            return;
        };
        let instance = cq.as_ref().map_or(0, |q| q.instance);
        let old = std::mem::replace(slot, cq);
        self.epochs.cq[idx].store(instance, Ordering::Release);
        if let Some(old) = old {
            self.release_held(old.held);
        }
    }

    /// Give back the tags of completions that will never be written.
    pub(super) fn release_held(
        &mut self,
        held: impl IntoIterator<Item = Held>,
    ) {
        for held in held {
            let slots = io_queue_index(held.cqe.sqid)
                .and_then(|idx| self.inflight.get_mut(idx));
            if let (Some(slots), Some(tag)) = (slots, held.tag) {
                slots.release(tag);
            }
        }
    }

    /// The head of SQ `sqid`, as a completion reports it.
    pub(super) fn sq_head(&self, sqid: u16) -> u16 {
        let sq = if sqid == ADMIN_QUEUE_ID {
            self.admin_sq.as_ref()
        } else {
            io_queue_index(sqid).and_then(|idx| self.io_sqs[idx].as_ref())
        };
        sq.map_or(0, |sq| sq.head)
    }

    /// Commands every I/O SQ has admitted and the backend has not
    /// finished.
    pub(super) fn in_flight(&self) -> u32 {
        self.inflight.iter().map(SqSlots::in_flight).sum()
    }

    pub(super) fn next_instance(&mut self) -> QueueInstance {
        self.instances += 1;
        self.instances
    }

    /// Record a fence deadline that passed with a transfer still in
    /// the backend.
    ///
    /// The controller is fatal from here and never comes back up. The
    /// worker that missed the deadline can still read or write the
    /// guest pages that a queue created after this would name, so
    /// nothing may be admitted again.
    pub(super) fn fail_fence(&mut self) {
        self.fence_failed = true;
        self.csts |= CSTS_CFS;
    }

    pub(super) fn set_shst(&mut self, shst: u32) {
        self.csts = (self.csts & !(CSTS_SHST_MASK << CSTS_SHST_SHIFT))
            | (shst << CSTS_SHST_SHIFT);
    }
}

/// Whether a guest-supplied queue base may be used.
///
/// Queues are contiguous (CAP.CQR is set), so the base must be page
/// aligned. Zero is never a queue.
pub(super) fn valid_queue_base(prp1: u64) -> bool {
    prp1 != 0 && (prp1 & 0xFFF) == 0
}

impl Default for NvmeState {
    fn default() -> Self {
        Self {
            cap: 0,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq_base: 0,
            acq_base: 0,
            admin_sq: None,
            admin_cq: None,
            io_sqs: Default::default(),
            io_cqs: Default::default(),
            ctrl_ident: [0u8; IDENT_CTRL_SIZE],
            ns_ident: [0u8; IDENT_NS_SIZE],
            num_io_queues: MAX_IO_QUEUES as u16,
            max_io_queues: MAX_IO_QUEUES as u16,
            volatile_write_cache: true,
            instances: 0,
            fence_failed: false,
            admission: Admission::default(),
            inflight: Default::default(),
            cq_inflight: [0; MAX_IO_QUEUES],
            throttled: 0,
            serial: 0,
            epochs: Arc::default(),
            drain_waiters: 0,
            irqs: Irqs::default(),
            retiring: 0,
        }
    }
}
