// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! I/O submission and execution.

use std::fs::File;
use std::io;
use std::sync::Arc;

use vmm_core::mem::{GuestIoVec, MemCtx, PhysMap};

use super::bits::*;
use super::prp::prp_to_iovec;
use super::queues::io_queue_index;
use super::{Geometry, NvmeController};

/// The store a namespace is backed by.
///
/// A trait rather than `File` so a test can hold a transfer open and
/// watch what a reset or a shutdown does with it.
pub trait IoBackend: Send + Sync + 'static {
    fn read_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize>;
    fn write_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize>;
    fn sync(&self) -> io::Result<()>;
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

/// One NVM command handed to a worker.
pub(super) struct IoRequest {
    pub(super) opcode: u8,
    pub(super) nsid: u32,
    pub(super) slba: u64,
    pub(super) nlb: u32,
    pub(super) prp1: u64,
    pub(super) prp2: u64,
    pub(super) sqid: u16,
    pub(super) cid: u16,
    pub(super) cq_id: u16,
    /// Force Unit Access. Identify advertises VWC=1, so a FUA write
    /// must reach stable storage before its completion.
    pub(super) fua: bool,
}

impl NvmeController {
    /// Process I/O SQ doorbell: parse SQEs and submit to worker channel.
    pub(super) fn io_sq_doorbell(&self, qid: u16, new_tail: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.state.lock().expect("nvme: state lock");

        let Some(idx) = io_queue_index(qid) else {
            return;
        };

        let sq = match &mut st.io_sqs[idx] {
            Some(sq) => sq,
            None => return,
        };

        let cq_id = sq.cq_id;
        if new_tail >= sq.size {
            return;
        }
        sq.tail = new_tail;

        // Collect all new SQEs before releasing the lock
        let mut requests = Vec::new();

        while sq.head != sq.tail {
            let sqe_offset = u64::from(sq.head) * SQE_SIZE as u64;
            let sqe_gpa = sq.base + sqe_offset;

            let mut sqe_buf = [0u8; SQE_SIZE];
            if mem.read(sqe_gpa, &mut sqe_buf).is_err() {
                sq.head = (sq.head + 1) % sq.size;
                continue;
            }

            let opcode = sqe_buf[SQE_OPC];
            let cid =
                u16::from_le_bytes([sqe_buf[SQE_CID], sqe_buf[SQE_CID + 1]]);
            let nsid = u32::from_le_bytes(
                sqe_buf[SQE_NSID..SQE_NSID + 4].try_into().expect("4 bytes"),
            );
            let prp1 = u64::from_le_bytes(
                sqe_buf[SQE_PRP1..SQE_PRP1 + 8].try_into().expect("8 bytes"),
            );
            let prp2 = u64::from_le_bytes(
                sqe_buf[SQE_PRP2..SQE_PRP2 + 8].try_into().expect("8 bytes"),
            );
            let cdw10 = u32::from_le_bytes(
                sqe_buf[SQE_CDW10..SQE_CDW10 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let cdw12 = u32::from_le_bytes(
                sqe_buf[SQE_CDW12..SQE_CDW12 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );

            // For NVM Read/Write: CDW10-11 = SLBA (64-bit), CDW12 bits 15:0 = NLB (0-based)
            let slba = u64::from(cdw10)
                | (u64::from(u32::from_le_bytes(
                    sqe_buf[SQE_CDW11..SQE_CDW11 + 4]
                        .try_into()
                        .expect("4 bytes"),
                )) << 32);
            let nlb = (cdw12 & 0xFFFF) + 1; // Convert 0-based to 1-based
                                            // NVMe 5.13: CDW12 bit 30 = Force Unit Access.
            let fua = (cdw12 & (1u32 << 30)) != 0;

            sq.head = (sq.head + 1) % sq.size;

            requests.push(IoRequest {
                opcode,
                nsid,
                slba,
                nlb,
                prp1,
                prp2,
                sqid: qid,
                cid,
                cq_id,
                fua,
            });
        }

        // Drop the state lock before sending to channel
        drop(st);

        // Send requests to worker threads
        let tx_guard = self.io_tx.lock().expect("nvme: io_tx lock");
        if let Some(tx) = tx_guard.as_ref() {
            for req in requests {
                // If the channel is disconnected, the workers have exited.
                // Silently drop the request.
                let _ = tx.send(req);
            }
        }
    }
}

/// Execute a single I/O request (Read, Write, or Flush).
///
/// Returns the NVMe status code for the completion entry.
pub(super) fn process_io_request(
    req: &IoRequest,
    backend: &dyn IoBackend,
    read_only: bool,
    physmap: &Arc<PhysMap>,
    geometry: Geometry,
) -> u16 {
    if req.nsid != 1 {
        return SC_INVALID_NS | SC_DNR;
    }

    let Geometry {
        block_size,
        total_blocks,
    } = geometry;

    let byte_count = match u64::from(req.nlb).checked_mul(u64::from(block_size))
    {
        Some(n) if n <= MAX_XFER_BYTES => n as usize,
        _ => return SC_INVALID_FIELD | SC_DNR,
    };
    let byte_offset = match req.slba.checked_mul(u64::from(block_size)) {
        Some(n) => n,
        None => return SC_INVALID_FIELD | SC_DNR,
    };
    if req
        .slba
        .checked_add(u64::from(req.nlb))
        .is_none_or(|end| end > total_blocks)
    {
        return SC_INVALID_FIELD | SC_DNR;
    }

    match req.opcode {
        NVM_OPC_READ => {
            let Ok(iov) = prp_to_iovec(physmap, req.prp1, req.prp2, byte_count)
            else {
                return SC_DATA_XFER_ERROR;
            };
            match backend.read_at(&iov, byte_offset) {
                Ok(n) if n == byte_count => SC_SUCCESS,
                _ => SC_DATA_XFER_ERROR,
            }
        }
        NVM_OPC_WRITE => {
            if read_only {
                return SC_DATA_XFER_ERROR;
            }
            let Ok(iov) = prp_to_iovec(physmap, req.prp1, req.prp2, byte_count)
            else {
                return SC_DATA_XFER_ERROR;
            };
            match backend.write_at(&iov, byte_offset) {
                Ok(n) if n == byte_count => {}
                _ => return SC_DATA_XFER_ERROR,
            }
            if req.fua && backend.sync().is_err() {
                return SC_INTERNAL_ERROR;
            }
            SC_SUCCESS
        }
        NVM_OPC_FLUSH => {
            if backend.sync().is_err() {
                return SC_INTERNAL_ERROR;
            }
            SC_SUCCESS
        }
        _ => SC_INVALID_OPCODE | SC_DNR,
    }
}
