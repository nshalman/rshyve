// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Admin command set.

use std::sync::Arc;

use vmm_core::mem::MemCtx;

use super::bits::*;
use super::prp::write_to_prp;
use super::queues::{
    io_queue_index, post_completion, valid_queue_base, CompQueue, NvmeState,
    SubQueue,
};
use super::NvmeController;

impl NvmeController {
    /// Process admin SQ doorbell: parse and execute admin commands inline.
    pub(super) fn admin_sq_doorbell(&self, new_tail: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.state.lock().expect("nvme: state lock");

        // Update the tail pointer (validate against queue size)
        match &mut st.admin_sq {
            Some(sq) => {
                if new_tail >= sq.size {
                    return;
                }
                sq.tail = new_tail;
            }
            None => return,
        }

        // Process entries in a loop, re-borrowing each iteration to
        // avoid holding a mutable borrow on admin_sq across the call
        // to process_admin_cmd (which borrows the full NvmeState).
        loop {
            // Read the next SQE from guest memory (borrow admin_sq briefly)
            let (sqe_gpa, sq_head, sq_size) = {
                let sq = match &st.admin_sq {
                    Some(sq) => sq,
                    None => return,
                };
                if sq.head == sq.tail {
                    break;
                }
                let sqe_offset = u64::from(sq.head) * SQE_SIZE as u64;
                (sq.base + sqe_offset, sq.head, sq.size)
            };

            let mut sqe_buf = [0u8; SQE_SIZE];
            if mem.read(sqe_gpa, &mut sqe_buf).is_err() {
                // Advance head past the bad entry
                if let Some(sq) = &mut st.admin_sq {
                    sq.head = (sq.head + 1) % sq.size;
                }
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
            let cdw11 = u32::from_le_bytes(
                sqe_buf[SQE_CDW11..SQE_CDW11 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );

            let sq_head = (sq_head + 1) % sq_size;
            if let Some(sq) = &mut st.admin_sq {
                sq.head = sq_head;
            }

            // Now process the command with full NvmeState access
            let (status, cdw0) = self.process_admin_cmd(
                &mut st, &mem, opcode, cid, nsid, prp1, prp2, cdw10, cdw11,
            );

            // Write CQE to admin CQ
            post_completion(
                &mem,
                &self.msix,
                &mut st.admin_cq,
                cdw0,
                ADMIN_QUEUE_ID,
                sq_head,
                cid,
                status,
            );
        }
    }

    /// Process a single admin command. Returns (status, cdw0_result).
    fn process_admin_cmd(
        &self,
        st: &mut NvmeState,
        mem: &MemCtx,
        opcode: u8,
        cid: u16,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let _ = cid;
        match opcode {
            ADMIN_OPC_IDENTIFY => {
                self.admin_identify(st, mem, nsid, prp1, prp2, cdw10)
            }
            ADMIN_OPC_CREATE_IO_CQ => {
                Self::admin_create_io_cq(st, prp1, cdw10, cdw11)
            }
            ADMIN_OPC_CREATE_IO_SQ => {
                Self::admin_create_io_sq(st, prp1, cdw10, cdw11)
            }
            ADMIN_OPC_DELETE_IO_SQ => self.admin_delete_io_sq(st, cdw10),
            ADMIN_OPC_DELETE_IO_CQ => self.admin_delete_io_cq(st, cdw10),
            ADMIN_OPC_GET_FEATURES => Self::admin_get_features(st, cdw10),
            ADMIN_OPC_SET_FEATURES => {
                Self::admin_set_features(st, cdw10, cdw11)
            }
            ADMIN_OPC_ABORT => {
                // Abort: always report "command not found" (success).
                // CDW0 bit 0 = 0 means the command was not found (no abort).
                (SC_SUCCESS, 0)
            }
            ADMIN_OPC_GET_LOG_PAGE => {
                // Minimal stub: return zeros
                let len = ((cdw10 >> 16) as usize + 1) * 4;
                let zeros = vec![0u8; len.min(4096)];
                let _ = mem.write(prp1, &zeros);
                (SC_SUCCESS, 0)
            }
            _ => (SC_INVALID_OPCODE | (SC_DNR), 0),
        }
    }

    /// Identify command (CNS 0 = namespace, CNS 1 = controller).
    fn admin_identify(
        &self,
        st: &NvmeState,
        mem: &MemCtx,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
    ) -> (u16, u32) {
        let cns = (cdw10 & 0xFF) as u8;
        let data: &[u8] = match cns {
            IDENTIFY_CNS_NAMESPACE => {
                if nsid != 1 {
                    return (SC_INVALID_NS | SC_DNR, 0);
                }
                &st.ns_ident
            }
            IDENTIFY_CNS_CONTROLLER => &st.ctrl_ident,
            _ => return (SC_INVALID_FIELD | SC_DNR, 0),
        };

        // Write 4096 bytes to guest memory via PRP
        if write_to_prp(mem, prp1, prp2, data).is_err() {
            return (SC_DATA_XFER_ERROR, 0);
        }
        (SC_SUCCESS, 0)
    }

    /// Create I/O Completion Queue.
    pub(super) fn admin_create_io_cq(
        st: &mut NvmeState,
        prp1: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let qsize = ((cdw10 >> 16) & 0xFFFF) + 1; // Convert 0-based to 1-based
        let iv = ((cdw11 >> 16) & 0xFFFF) as u16;

        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if qsize < 2 || qsize > u32::from(MAX_QUEUE_SIZE) {
            return (SC_INVALID_QUEUE_SIZE | SC_DNR, 0);
        }
        let qsize = qsize as u16;

        if st.io_cqs[idx].is_some() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }

        if !valid_queue_base(prp1) {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        st.io_cqs[idx] = Some(CompQueue {
            base: prp1,
            size: qsize,
            head: 0,
            tail: 0,
            phase: true,
            iv,
        });

        (SC_SUCCESS, 0)
    }

    /// Create I/O Submission Queue.
    pub(super) fn admin_create_io_sq(
        st: &mut NvmeState,
        prp1: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let qsize = ((cdw10 >> 16) & 0xFFFF) + 1;
        let cq_id = ((cdw11 >> 16) & 0xFFFF) as u16;

        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if qsize < 2 || qsize > u32::from(MAX_QUEUE_SIZE) {
            return (SC_INVALID_QUEUE_SIZE | SC_DNR, 0);
        }
        let qsize = qsize as u16;

        if st.io_sqs[idx].is_some() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }

        // Verify the associated CQ exists
        let Some(cq_idx) = io_queue_index(cq_id) else {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        };
        if st.io_cqs[cq_idx].is_none() {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        if !valid_queue_base(prp1) {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        st.io_sqs[idx] = Some(SubQueue {
            base: prp1,
            size: qsize,
            head: 0,
            tail: 0,
            cq_id,
        });

        (SC_SUCCESS, 0)
    }

    /// Delete I/O Submission Queue.
    fn admin_delete_io_sq(&self, st: &mut NvmeState, cdw10: u32) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if st.io_sqs[idx].is_none() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        st.io_sqs[idx] = None;
        (SC_SUCCESS, 0)
    }

    /// Delete I/O Completion Queue.
    fn admin_delete_io_cq(&self, st: &mut NvmeState, cdw10: u32) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if st.io_cqs[idx].is_none() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        // Verify no SQs reference this CQ
        for sq in st.io_sqs.iter().flatten() {
            if sq.cq_id == qid {
                return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
            }
        }
        st.io_cqs[idx] = None;
        (SC_SUCCESS, 0)
    }

    /// Get Features command.
    pub(super) fn admin_get_features(st: &NvmeState, cdw10: u32) -> (u16, u32) {
        let fid = (cdw10 & 0xFF) as u8;
        match fid {
            FEAT_NUM_QUEUES => {
                // CDW0 bits 31:16 = num CQs allocated (0-based)
                // CDW0 bits 15:0  = num SQs allocated (0-based)
                let nq = u32::from(st.num_io_queues.saturating_sub(1));
                (SC_SUCCESS, nq | (nq << 16))
            }
            FEAT_VOLATILE_WC => {
                // Volatile write cache: report enabled
                (SC_SUCCESS, 1)
            }
            FEAT_TEMP_THRESHOLD | FEAT_ERROR_RECOVERY | FEAT_POWER_MGMT
            | FEAT_ARBITRATION | FEAT_INTR_COALESCING
            | FEAT_INTR_VECTOR_CFG | FEAT_WRITE_ATOMICITY
            | FEAT_ASYNC_EVENT_CFG => (SC_SUCCESS, 0),
            _ => (SC_INVALID_FIELD | SC_DNR, 0),
        }
    }

    /// Set Features command.
    pub(super) fn admin_set_features(
        st: &mut NvmeState,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let fid = (cdw10 & 0xFF) as u8;
        match fid {
            FEAT_NUM_QUEUES => {
                // Guest requests N SQs and N CQs (0-based in CDW11)
                let requested_sq = (cdw11 & 0xFFFF) + 1;
                let requested_cq = ((cdw11 >> 16) & 0xFFFF) + 1;
                let max = MAX_IO_QUEUES as u32;
                let alloc_sq = requested_sq.min(max);
                let alloc_cq = requested_cq.min(max);
                st.num_io_queues = alloc_sq.min(alloc_cq) as u16;
                let result_sq = alloc_sq.saturating_sub(1);
                let result_cq = alloc_cq.saturating_sub(1);
                (SC_SUCCESS, result_sq | (result_cq << 16))
            }
            FEAT_VOLATILE_WC => (SC_SUCCESS, 0),
            FEAT_TEMP_THRESHOLD | FEAT_ERROR_RECOVERY | FEAT_POWER_MGMT
            | FEAT_ARBITRATION | FEAT_INTR_COALESCING
            | FEAT_INTR_VECTOR_CFG | FEAT_WRITE_ATOMICITY
            | FEAT_ASYNC_EVENT_CFG => (SC_SUCCESS, 0),
            _ => (SC_INVALID_FIELD | SC_DNR, 0),
        }
    }
}
