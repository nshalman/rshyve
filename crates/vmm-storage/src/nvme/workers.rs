// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! I/O worker threads.

use std::fs::File;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use vmm_core::mem::{MemCtx, PhysMap};
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::QuiesceGate;

use super::io::{process_io_request, IoRequest};
use super::queues::{io_queue_index, post_completion};
use super::NvmeController;

// ---------------------------------------------------------------------------
// I/O worker thread
// ---------------------------------------------------------------------------

/// Worker thread loop: reads I/O requests from the channel and executes them.
///
/// Uses `pread`/`pwrite` on the raw fd for lock-free concurrent I/O.
/// Each completed request posts a CQE to guest memory and fires MSI-X.
pub(super) fn io_worker(
    rx: Arc<Mutex<mpsc::Receiver<IoRequest>>>,
    file: Arc<File>,
    read_only: bool,
    physmap: Arc<PhysMap>,
    msix: Arc<MsixTable>,
    ctrl: Arc<NvmeController>,
    gate: Arc<QuiesceGate>,
) {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        // Acquire the receiver lock only to get the next message
        let req = {
            let rx_guard = rx.lock().expect("nvme: io_rx lock");
            match rx_guard.recv_timeout(Duration::from_millis(50)) {
                Ok(req) => req,
                Err(RecvTimeoutError::Timeout) => {
                    drop(rx_guard);
                    gate.park_if_paused();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        let status =
            process_io_request(&req, &file, read_only, &physmap, &ctrl);
        // Post completion: need to lock state briefly to access the CQ
        let mem = MemCtx::new(Arc::clone(&physmap));
        let sq_head = {
            let st = ctrl.state.lock().expect("nvme: state lock");
            if let Some(idx) = io_queue_index(req.sqid) {
                st.io_sqs[idx].as_ref().map(|sq| sq.head).unwrap_or(0)
            } else {
                0
            }
        };

        let mut st = ctrl.state.lock().expect("nvme: state lock");
        if let Some(cq_idx) = io_queue_index(req.cq_id) {
            post_completion(
                &mem,
                &msix,
                &mut st.io_cqs[cq_idx],
                0,
                req.sqid,
                sq_head,
                req.cid,
                status,
            );
        }
    }
}
