// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Queue state and completion posting.

use vmm_core::mem::MemCtx;
use vmm_devices::pci::msix::MsixTable;

use super::bits::*;

// ---------------------------------------------------------------------------
// Queue structures
// ---------------------------------------------------------------------------

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
    pub(super) block_size: u32,
    pub(super) total_blocks: u64,
}

// ---------------------------------------------------------------------------
// Completion queue helpers
// ---------------------------------------------------------------------------

/// Post a completion queue entry to guest memory and fire MSI-X.
pub(super) fn post_completion(
    mem: &MemCtx,
    msix: &MsixTable,
    cq_opt: &mut Option<CompQueue>,
    cdw0: u32,
    sqid: u16,
    sq_head: u16,
    cid: u16,
    status: u16,
) {
    let cq = match cq_opt.as_mut() {
        Some(cq) => cq,
        None => return,
    };

    // Check if CQ is full: (tail + 1) % size == head
    let next_tail = if cq.tail + 1 >= cq.size {
        0
    } else {
        cq.tail + 1
    };
    if next_tail == cq.head {
        // CQ full -- drop the completion (guest must drain the queue)
        return;
    }

    let cqe_offset = u64::from(cq.tail) * CQE_SIZE as u64;
    let cqe_gpa = cq.base + cqe_offset;

    // Build CQE (16 bytes)
    let mut cqe = [0u8; CQE_SIZE];

    // DW0: command-specific result
    cqe[CQE_DW0..CQE_DW0 + 4].copy_from_slice(&cdw0.to_le_bytes());
    // DW1: reserved (already zero)

    // DW2: SQ head pointer + SQ ID
    cqe[CQE_SQHD..CQE_SQHD + 2].copy_from_slice(&sq_head.to_le_bytes());
    cqe[CQE_SQID..CQE_SQID + 2].copy_from_slice(&sqid.to_le_bytes());

    // DW3: CID + Status (with phase bit in bit 0 of status field)
    cqe[CQE_CID..CQE_CID + 2].copy_from_slice(&cid.to_le_bytes());

    // Status field: bits 15:1 = status code, bit 0 = phase
    let status_with_phase = (status << 1) | if cq.phase { 1 } else { 0 };
    cqe[CQE_STATUS..CQE_STATUS + 2]
        .copy_from_slice(&status_with_phase.to_le_bytes());

    // Write CQE to guest memory
    let _ = mem.write(cqe_gpa, &cqe);

    // Advance CQ tail and toggle phase on wrap
    let iv = cq.iv;
    cq.tail += 1;
    if cq.tail >= cq.size {
        cq.tail = 0;
        cq.phase = !cq.phase;
    }

    // Fire MSI-X interrupt
    msix.fire(iv);
}

/// Whether a guest-supplied queue base may be used: page-aligned and
/// non-zero. A malicious guest could otherwise point a queue at the
/// MMIO region of another device.
pub(super) fn valid_queue_base(prp1: u64) -> bool {
    prp1 != 0 && (prp1 & 0xFFF) == 0
}

// ---------------------------------------------------------------------------
// Default impl for Option arrays
// ---------------------------------------------------------------------------

// SubQueue and CompQueue don't implement Copy/Clone, so we need a manual
// Default for the fixed-size arrays.
impl Default for NvmeState {
    fn default() -> Self {
        // This is only used internally; the real init is in NvmeController::new
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
            block_size: 512,
            total_blocks: 0,
        }
    }
}
