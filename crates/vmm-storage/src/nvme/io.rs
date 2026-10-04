// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! I/O submission: the SQ tail doorbell, and the checks a command
//! passes before a backend sees it.

use std::fs::File;
use std::io;
use std::sync::Arc;

use vmm_core::mem::{GuestIoVec, MemCtx};

use super::backend::{BlockOp, BlockRequest, GuestData, Liveness, RequestId};
use super::bits::*;
use super::inflight::Admitted;
use super::queues::{io_queue_index, NvmeState};
use super::NvmeController;

/// A synchronous store, run on the worker pool.
///
/// A trait rather than `File` so a test can hold a transfer open and
/// watch what a reset or a shutdown does with it.
pub trait IoBackend: Send + Sync + 'static {
    fn read_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize>;
    fn write_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize>;
    fn sync(&self) -> io::Result<()>;

    /// Whether a write is durable when `write_at` returns.
    fn durable_writes(&self) -> bool {
        false
    }
}

impl IoBackend for File {
    fn read_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize> {
        iov.read_from(self, offset)
    }

    fn write_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize> {
        iov.write_to(self, offset)
    }

    fn sync(&self) -> io::Result<()> {
        self.sync_data()
    }
}

/// The fields of an NVM command SQE.
struct NvmCmd {
    opcode: u8,
    cid: u16,
    nsid: u32,
    slba: u64,
    /// 1-based.
    nlb: u32,
    prp1: u64,
    prp2: u64,
    fua: bool,
}

fn le_u32(sqe: &[u8; SQE_SIZE], at: usize) -> u32 {
    u32::from_le_bytes([sqe[at], sqe[at + 1], sqe[at + 2], sqe[at + 3]])
}

fn le_u64(sqe: &[u8; SQE_SIZE], at: usize) -> u64 {
    u64::from(le_u32(sqe, at)) | (u64::from(le_u32(sqe, at + 4)) << 32)
}

impl NvmCmd {
    fn decode(sqe: &[u8; SQE_SIZE]) -> Self {
        let cdw12 = le_u32(sqe, SQE_CDW12);
        Self {
            opcode: sqe[SQE_OPC],
            cid: u16::from_le_bytes([sqe[SQE_CID], sqe[SQE_CID + 1]]),
            nsid: le_u32(sqe, SQE_NSID),
            // CDW10 and CDW11 hold the 64-bit starting LBA.
            slba: le_u64(sqe, SQE_CDW10),
            // CDW12 bits 15:0 are the 0-based block count.
            nlb: (cdw12 & 0xFFFF) + 1,
            prp1: le_u64(sqe, SQE_PRP1),
            prp2: le_u64(sqe, SQE_PRP2),
            // NVMe 5.13: CDW12 bit 30 is Force Unit Access.
            fua: (cdw12 & (1u32 << 30)) != 0,
        }
    }
}

/// A command that passed its checks.
struct Checked {
    op: BlockOp,
    byte_offset: u64,
    len: usize,
}

impl NvmeController {
    /// The I/O SQ tail doorbell.
    pub(super) fn io_sq_doorbell(&self, qid: u16, new_tail: u16) {
        let Some(idx) = io_queue_index(qid) else {
            return;
        };
        let mut st = self.lock();
        let Some(sq) = st.io_sqs[idx].as_mut() else {
            return;
        };
        if new_tail >= sq.size {
            return;
        }
        sq.tail = new_tail;
        self.admit(&mut st, idx);
    }

    /// Hand the backend what the guest has queued on I/O SQ `idx`, as
    /// far as admission and the queue's depth allow.
    ///
    /// The checks and the submit happen under the state lock, the same
    /// lock that closing admission takes, so a drain that counted
    /// nothing in flight is never overtaken. A command not admitted
    /// stays in the guest's queue with the head on it.
    pub(super) fn admit(&self, st: &mut NvmeState, idx: usize) {
        if !st.admission.is_open() || st.fence_failed {
            return;
        }
        st.throttled &= !(1u16 << idx);
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        loop {
            let Some(sq) = st.io_sqs[idx].as_ref() else {
                return;
            };
            if sq.head == sq.tail {
                return;
            }
            let (cq_id, sq_instance) = (sq.cq_id, sq.instance);
            // Nothing submitted here can complete without its CQ.
            let Some((cq_idx, cq_instance)) =
                io_queue_index(cq_id).and_then(|cq_idx| {
                    st.io_cqs[cq_idx].as_ref().map(|cq| (cq_idx, cq.instance))
                })
            else {
                return;
            };
            if !st.inflight[idx].has_room(self.depth) {
                st.throttled |= 1u16 << idx;
                return;
            }

            let Some(sq) = st.io_sqs[idx].as_mut() else {
                return;
            };
            let entry =
                sq.base.checked_add(u64::from(sq.head) * SQE_SIZE as u64);
            sq.head = (sq.head + 1) % sq.size;
            let mut sqe = [0u8; SQE_SIZE];
            if entry.is_none_or(|gpa| mem.read(gpa, &mut sqe).is_err()) {
                continue;
            }
            let cmd = NvmCmd::decode(&sqe);

            st.serial += 1;
            let serial = st.serial;
            let admitted = Admitted {
                serial,
                cid: cmd.cid,
                opcode: cmd.opcode,
                cq_id,
                sq_instance,
                cq_instance,
            };
            let Some(tag) = st.inflight[idx].admit(self.depth, admitted) else {
                return;
            };
            st.cq_inflight[cq_idx] += 1;
            self.counts.admitted();
            let id = RequestId {
                queue: idx as u16 + 1,
                tag,
                serial,
            };

            match self.check(&cmd) {
                Err(status) => self.finish_now(st, &mem, id, status),
                Ok(Checked {
                    op,
                    byte_offset,
                    len,
                }) => self.backend.submit(BlockRequest {
                    id,
                    op,
                    lba: if op == BlockOp::Flush { 0 } else { cmd.slba },
                    blocks: if op == BlockOp::Flush { 0 } else { cmd.nlb },
                    byte_offset,
                    data: GuestData {
                        mem: MemCtx::new(Arc::clone(&self.physmap)),
                        prp1: cmd.prp1,
                        prp2: cmd.prp2,
                        len,
                        live: Liveness {
                            epochs: Arc::clone(&st.epochs),
                            sq: idx,
                            sq_instance,
                            cq: cq_idx,
                            cq_instance,
                        },
                    },
                }),
            }
        }
    }

    /// Admit on every I/O SQ the guest has left commands in.
    pub(super) fn admit_all(&self, st: &mut NvmeState) {
        for idx in 0..MAX_IO_QUEUES {
            let pending =
                st.io_sqs[idx].as_ref().is_some_and(|sq| sq.head != sq.tail);
            if pending {
                self.admit(st, idx);
            }
        }
    }

    /// Check a command against the namespace, or say how it fails.
    fn check(&self, cmd: &NvmCmd) -> Result<Checked, u16> {
        if cmd.nsid != 1 {
            return Err(SC_INVALID_NS | SC_DNR);
        }
        let block = u64::from(self.geometry.block_size);
        let len = match u64::from(cmd.nlb).checked_mul(block) {
            Some(n) if n <= MAX_XFER_BYTES => n as usize,
            _ => return Err(SC_INVALID_FIELD | SC_DNR),
        };
        let Some(byte_offset) = cmd.slba.checked_mul(block) else {
            return Err(SC_INVALID_FIELD | SC_DNR);
        };
        if cmd
            .slba
            .checked_add(u64::from(cmd.nlb))
            .is_none_or(|end| end > self.geometry.total_blocks)
        {
            return Err(SC_INVALID_FIELD | SC_DNR);
        }
        let op = match cmd.opcode {
            NVM_OPC_READ => BlockOp::Read,
            NVM_OPC_WRITE if self.read_only => {
                return Err(SC_WRITE_TO_RO_RANGE | SC_DNR);
            }
            NVM_OPC_WRITE => BlockOp::Write { fua: cmd.fua },
            NVM_OPC_FLUSH => {
                return Ok(Checked {
                    op: BlockOp::Flush,
                    byte_offset: 0,
                    len: 0,
                });
            }
            _ => return Err(SC_INVALID_OPCODE | SC_DNR),
        };
        Ok(Checked {
            op,
            byte_offset,
            len,
        })
    }
}
