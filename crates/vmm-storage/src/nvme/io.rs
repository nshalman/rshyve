// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! I/O submission, and the checks a command passes before a backend
//! sees it.

use std::io;
use std::sync::Arc;

use vmm_core::mem::{GuestIoVec, MemCtx};
use vmm_devices::blkdev::DiskSync;

use super::backend::{BlockOp, BlockRequest, GuestData, Liveness, RequestId};
use super::bits::*;
use super::inflight::Admitted;
use super::prp;
use super::queues::{io_queue_index, NvmeState};
use super::NvmeController;

/// A synchronous store, run on the worker pool.
///
/// A trait rather than [`DiskSync`] so a test can hold a transfer open and
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

impl IoBackend for DiskSync {
    fn read_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize> {
        iov.read_from(self.file(), offset)
    }

    fn write_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize> {
        iov.write_to(self.file(), offset)
    }

    fn sync(&self) -> io::Result<()> {
        DiskSync::sync(self)
    }
}

struct NvmCmd {
    opcode: u8,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
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
            cdw10: le_u32(sqe, SQE_CDW10),
            cdw11: le_u32(sqe, SQE_CDW11),
            cdw12,
            cid: u16::from_le_bytes([sqe[SQE_CID], sqe[SQE_CID + 1]]),
            nsid: le_u32(sqe, SQE_NSID),
            slba: le_u64(sqe, SQE_CDW10),
            // CDW12 bits 15:0 are the 0-based block count.
            nlb: (cdw12 & 0xFFFF) + 1,
            prp1: le_u64(sqe, SQE_PRP1),
            prp2: le_u64(sqe, SQE_PRP2),
            // CDW12 bit 30 is Force Unit Access.
            fua: (cdw12 & (1u32 << 30)) != 0,
        }
    }
}

struct Checked {
    op: BlockOp,
    byte_offset: u64,
    len: usize,
    zero: Vec<(u64, u64)>,
}

impl NvmeController {
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

    /// The checks and the submit run under the state lock, which closing
    /// admission also takes, so a drain that counted nothing in flight
    /// is never overtaken. A command not admitted stays in the guest's
    /// queue with the head on it.
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
            self.counts.admitted();
            let id = RequestId {
                queue: idx as u16 + 1,
                tag,
                serial,
            };

            match self.check(&cmd, &mem) {
                Err(status) => self.finish_now(st, &mem, id, status),
                // With no volatile cache every completed write is
                // already durable, so there is nothing to flush.
                Ok(Checked {
                    op: BlockOp::Flush, ..
                }) if !st.volatile_write_cache => {
                    self.finish_now(st, &mem, id, SC_SUCCESS)
                }
                // A Dataset Management with nothing to deallocate.
                Ok(Checked { op, ref zero, .. })
                    if op.is_zeroing() && zero.is_empty() =>
                {
                    self.finish_now(st, &mem, id, SC_SUCCESS)
                }
                Ok(Checked {
                    op,
                    byte_offset,
                    len,
                    zero,
                }) => self.submit(
                    st,
                    &mem,
                    BlockRequest {
                        id,
                        op,
                        // A Dataset Management names its blocks in
                        // `zero`, not in its command words.
                        lba: if op == BlockOp::Flush
                            || op == BlockOp::Deallocate
                        {
                            0
                        } else {
                            cmd.slba
                        },
                        blocks: if op == BlockOp::Flush
                            || op == BlockOp::Deallocate
                        {
                            0
                        } else {
                            cmd.nlb
                        },
                        zero,
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
                    },
                ),
            }
        }
    }

    pub(super) fn admit_all(&self, st: &mut NvmeState) {
        for idx in 0..MAX_IO_QUEUES {
            let pending =
                st.io_sqs[idx].as_ref().is_some_and(|sq| sq.head != sq.tail);
            if pending {
                self.admit(st, idx);
            }
        }
    }

    fn check(&self, cmd: &NvmCmd, mem: &MemCtx) -> Result<Checked, u16> {
        if cmd.nsid != 1 {
            return Err(SC_INVALID_NS | SC_DNR);
        }
        let zeroing = matches!(cmd.opcode, NVM_OPC_DSM | NVM_OPC_WRITE_ZEROES);
        if zeroing && !self.config.deallocate {
            return Err(SC_INVALID_OPCODE | SC_DNR);
        }
        if zeroing && self.read_only {
            return Err(SC_WRITE_TO_RO_RANGE | SC_DNR);
        }
        if cmd.opcode == NVM_OPC_DSM {
            return self.check_dsm(cmd, mem);
        }
        let block = u64::from(self.geometry.block_size);
        let Some(bytes) = u64::from(cmd.nlb).checked_mul(block) else {
            return Err(SC_INVALID_FIELD | SC_DNR);
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
        if cmd.opcode == NVM_OPC_WRITE_ZEROES {
            // No data moves, so MDTS does not apply.
            return Ok(Checked {
                op: BlockOp::WriteZeroes {
                    deallocate: cmd.cdw12 & WZ_DEAC != 0,
                    fua: cmd.fua,
                },
                byte_offset,
                len: 0,
                zero: vec![(byte_offset, bytes)],
            });
        }
        let len = match bytes {
            n if n <= MAX_XFER_BYTES => n as usize,
            _ => return Err(SC_INVALID_FIELD | SC_DNR),
        };
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
                    zero: Vec::new(),
                });
            }
            _ => return Err(SC_INVALID_OPCODE | SC_DNR),
        };
        Ok(Checked {
            op,
            byte_offset,
            len,
            zero: Vec::new(),
        })
    }

    /// Dataset Management: the range list is read once, here, so the
    /// guest cannot change it under the backend. Only the deallocate
    /// attribute does anything; the others are hints.
    fn check_dsm(&self, cmd: &NvmCmd, mem: &MemCtx) -> Result<Checked, u16> {
        let done = |zero| {
            Ok(Checked {
                op: BlockOp::Deallocate,
                byte_offset: 0,
                len: 0,
                zero,
            })
        };
        if cmd.cdw11 & DSM_AD == 0 {
            return done(Vec::new());
        }
        let ranges = (cmd.cdw10 & 0xff) as usize + 1;
        let mut list = vec![0u8; ranges * DSM_RANGE_BYTES];
        let len = list.len();
        if prp::copy_from_prp(mem, cmd.prp1, cmd.prp2, len, &mut list).is_err()
        {
            return Err(SC_DATA_XFER_ERROR);
        }
        let block = u64::from(self.geometry.block_size);
        let mut zero = Vec::with_capacity(ranges);
        let (whole, _) = list.as_chunks::<DSM_RANGE_BYTES>();
        for r in whole {
            let nlb = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
            let slba =
                u64::from_le_bytes(r[8..16].try_into().unwrap_or([0; 8]));
            if nlb == 0 {
                continue;
            }
            if slba
                .checked_add(u64::from(nlb))
                .is_none_or(|end| end > self.geometry.total_blocks)
            {
                return Err(SC_LBA_OUT_OF_RANGE | SC_DNR);
            }
            // Both fit: the namespace size in bytes fits a u64.
            zero.push((slba * block, u64::from(nlb) * block));
        }
        done(zero)
    }
}
