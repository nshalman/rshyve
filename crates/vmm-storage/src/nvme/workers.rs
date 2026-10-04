// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! I/O worker threads.

use std::sync::{Arc, Weak};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use vmm_core::mem::MemCtx;
use vmm_devices::QuiesceGate;

use super::bits::CSTS_CFS;
use super::io::{process_io_request, queues_current, IoBackend, IoRequest};
use super::queues::{io_queue_index, post_completion};
use super::NvmeController;

/// How long a worker waits on an empty channel before looking at the
/// gate again. Only an idle controller pays it.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Worker thread loop: reads I/O requests from the channel and executes them.
///
/// The controller reference is weak so the worker pool never keeps the
/// controller, its backing fd or its `DiskCache` alive.
pub(super) fn io_worker(
    rx: Receiver<IoRequest>,
    backend: Arc<dyn IoBackend>,
    read_only: bool,
    ctrl: Weak<NvmeController>,
    gate: Arc<QuiesceGate>,
) {
    loop {
        // Park before taking new work. A guest that keeps the channel
        // non-empty would otherwise never let a worker reach the gate,
        // and an operator pause would time out under load.
        gate.park_if_paused();

        let req = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(req) => req,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        // A pause between the dequeue and the transfer stops the
        // transfer, so a drain covers the guest memory this touches.
        gate.park_if_paused();

        let Some(ctrl) = ctrl.upgrade() else {
            return;
        };

        // A reset or a Delete Queue between the submission and here
        // took the pages back; the fence in those paths waits for
        // whatever got past this check.
        if !queues_current(&ctrl.state.lock().expect("nvme: state lock"), &req)
        {
            slog::debug!(ctrl.log, "nvme: dropped I/O for a retired queue";
                "sqid" => req.sqid, "cid" => req.cid);
            continue;
        }

        let mem = MemCtx::new(Arc::clone(&ctrl.physmap));
        let status = process_io_request(
            &req,
            backend.as_ref(),
            read_only,
            &mem,
            ctrl.geometry,
        );

        let mut st = ctrl.state.lock().expect("nvme: state lock");
        if !queues_current(&st, &req) {
            continue;
        }
        let sq_head = io_queue_index(req.sqid)
            .and_then(|idx| st.io_sqs[idx].as_ref())
            .map_or(0, |sq| sq.head);
        let Some(cq_idx) = io_queue_index(req.cq_id) else {
            continue;
        };
        if let Err(error) = post_completion(
            &mem,
            &ctrl.msix,
            &mut st.io_cqs[cq_idx],
            0,
            req.sqid,
            sq_head,
            req.cid,
            status,
        ) {
            // The driver will never see this command finish.
            slog::error!(ctrl.log, "nvme: I/O CQE write failed";
                "error" => %error);
            st.csts |= CSTS_CFS;
        }
    }
}
