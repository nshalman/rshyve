// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Writing completions into a guest completion queue.
//!
//! A completion is never dropped. When the ring is full it waits in
//! the queue's held list for the guest's CQ head doorbell, keeping its
//! command's tag so the controller admits no more than it can hold.
//!
//! Writing a completion only notes the interrupt it owes; the state
//! lock raises them once it is released.

use std::io;

use vmm_core::mem::MemCtx;
use vmm_devices::pci::msix::MsixTable;

use super::bits::*;
use super::queues::{CompQueue, NvmeState};
use super::MsiPolicy;

/// A completion the controller owes the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Cqe {
    pub(super) cdw0: u32,
    pub(super) sqid: u16,
    pub(super) cid: u16,
    /// The status field without the phase bit.
    pub(super) status: u16,
}

/// A completion waiting for space in its CQ.
#[derive(Debug, Clone, Copy)]
pub(super) struct Held {
    pub(super) cqe: Cqe,
    /// The command's tag in its SQ, given back once this is written.
    /// An admin command has none.
    pub(super) tag: Option<u16>,
}

/// What became of a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Posted {
    Written,
    Held,
}

impl CompQueue {
    fn is_full(&self) -> bool {
        let next = if self.tail + 1 >= self.size {
            0
        } else {
            self.tail + 1
        };
        next == self.head
    }

    /// Whether a new completion would be written now, not held.
    pub(super) fn has_room(&self) -> bool {
        self.held.is_empty() && !self.is_full()
    }

    /// Write `cqe` at the tail, then advance it, flipping the phase on
    /// a wrap.
    fn write(
        &mut self,
        mem: &MemCtx,
        cqe: Cqe,
        sq_head: u16,
    ) -> io::Result<()> {
        let Some(gpa) = self
            .base
            .checked_add(u64::from(self.tail) * CQE_SIZE as u64)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CQ base wraps",
            ));
        };

        let mut entry = [0u8; CQE_SIZE];
        entry[CQE_DW0..CQE_DW0 + 4].copy_from_slice(&cqe.cdw0.to_le_bytes());
        entry[CQE_SQHD..CQE_SQHD + 2].copy_from_slice(&sq_head.to_le_bytes());
        entry[CQE_SQID..CQE_SQID + 2].copy_from_slice(&cqe.sqid.to_le_bytes());
        entry[CQE_CID..CQE_CID + 2].copy_from_slice(&cqe.cid.to_le_bytes());
        // Status bits 15:1, phase tag bit 0.
        let status = (cqe.status << 1) | u16::from(self.phase);
        entry[CQE_STATUS..CQE_STATUS + 2]
            .copy_from_slice(&status.to_le_bytes());
        mem.write(gpa, &entry)?;

        self.tail += 1;
        if self.tail >= self.size {
            self.tail = 0;
            self.phase = !self.phase;
        }
        Ok(())
    }
}

/// Interrupts owed for the completions written under the state lock,
/// one slot per CQ: the admin CQ, then I/O CQs 1 to 15.
#[derive(Default)]
pub(super) struct Irqs {
    owed: [(u16, u16); MAX_IO_QUEUES + 1],
}

impl Irqs {
    /// A completion went into CQ `qid`.
    fn note(&mut self, qid: u16, cq: &CompQueue) {
        if !cq.ien {
            return;
        }
        if let Some((count, vector)) = self.owed.get_mut(usize::from(qid)) {
            *count = count.saturating_add(1);
            *vector = cq.iv;
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.owed.iter().all(|&(count, _)| count == 0)
    }

    /// One MSI per CQ that got completions, or one per completion.
    pub(super) fn raise(&self, msix: &MsixTable, policy: MsiPolicy) {
        for &(count, vector) in &self.owed {
            let times = match policy {
                MsiPolicy::PerBatch => count.min(1),
                MsiPolicy::PerCompletion => count,
            };
            for _ in 0..times {
                msix.fire(vector);
            }
        }
    }
}

/// Write `cqe` into CQ `qid`, owing its interrupt, or hold it behind
/// the completions already waiting.
///
/// An error means the CQE could not be written, which loses the
/// completion: the caller reports a fatal controller status.
pub(super) fn post_or_hold(
    mem: &MemCtx,
    irqs: &mut Irqs,
    qid: u16,
    cq: &mut CompQueue,
    cqe: Cqe,
    sq_head: u16,
    tag: Option<u16>,
) -> io::Result<Posted> {
    if !cq.has_room() {
        cq.held.push_back(Held { cqe, tag });
        return Ok(Posted::Held);
    }
    cq.write(mem, cqe, sq_head)?;
    irqs.note(qid, cq);
    Ok(Posted::Written)
}

impl NvmeState {
    /// Write what CQ `idx` holds while it has space, `None` being the
    /// admin CQ, and give back the tags of what was written.
    pub(super) fn flush_held(
        &mut self,
        mem: &MemCtx,
        idx: Option<usize>,
    ) -> io::Result<()> {
        let mut heads = [0u16; MAX_IO_QUEUES + 1];
        for (sqid, head) in heads.iter_mut().enumerate() {
            *head = self.sq_head(sqid as u16);
        }
        let qid = idx.map_or(ADMIN_QUEUE_ID, |idx| idx as u16 + 1);
        let cq = match idx {
            None => self.admin_cq.as_mut(),
            Some(idx) => self.io_cqs.get_mut(idx).and_then(Option::as_mut),
        };
        let Some(cq) = cq else {
            return Ok(());
        };

        let mut result = Ok(());
        let mut done = Vec::new();
        while !cq.is_full() {
            let Some(held) = cq.held.pop_front() else {
                break;
            };
            let sq_head =
                heads.get(usize::from(held.cqe.sqid)).copied().unwrap_or(0);
            match cq.write(mem, held.cqe, sq_head) {
                Ok(()) => self.irqs.note(qid, cq),
                Err(error) => result = Err(error),
            }
            done.push(held);
        }
        self.release_held(done);
        result
    }
}
