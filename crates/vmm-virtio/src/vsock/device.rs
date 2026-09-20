// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The virtio-vsock device and its host sockets.
//!
//! The host interface is Firecracker's, so tooling written against that
//! works unchanged:
//!
//! - host to guest: connect to the device socket, send
//!   `CONNECT <port>\n`, read back `OK <host_port>\n`, then stream.
//! - guest to host: the guest connects to port N and the device
//!   connects to `<socket>_N`.
//!
//! Threads rather than an async runtime: `deny-firehyve.toml` keeps
//! tokio out of firehyve's dependency graph on purpose, and the console
//! already solves the same shape this way.
//!
//! This file is the device seen from the transport: the ring callbacks
//! and the config space. One responsibility per submodule, and each
//! names the invariant it owns:
//!
//! - [`shared`]: the state every thread reaches.
//! - [`sockets`]: the `ConnKey` tables and the threads serving them.
//! - [`rx`]: host to guest.
//! - [`tx`]: guest to host.
//! - [`listener`]: the host socket, its accept loop and its readers.
//! - [`halt`]: pause, resume and the bounded teardown.

mod halt;
mod listener;
mod rx;
mod shared;
mod sockets;
mod tx;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::Context;
use vmm_core::mem::PhysMap;
use vmm_core::unixsock::{bind_restricted, SocketPolicy};
use vmm_devices::lifecycle::Indicator;
use vmm_devices::QuiesceGate;

use self::shared::VsockShared;
use super::control::ControlSlot;
use super::mux::VsockMux;
use super::packet::CID_GUEST_MIN;
use crate::pci::intr::{BackendIntr, IntrSlot};
use crate::queue::VirtQueue;
use crate::VirtioDevice;

pub use self::listener::parse_connect_line;

/// rx, tx and event. Under the transport's four-queue cap.
pub const VSOCK_NUM_QUEUES: usize = 3;
/// Host to guest.
pub const VSOCK_RX_QUEUE: u16 = 0;
/// Guest to host.
pub const VSOCK_TX_QUEUE: u16 = 1;
/// Transport reset events. Buffers are accepted and never used.
pub const VSOCK_EVENT_QUEUE: u16 = 2;
/// Must be a power of two: `VirtQueue::new` panics otherwise.
pub const VSOCK_QUEUE_SIZE: u16 = 256;
/// `struct virtio_vsock_config` is one le64 guest_cid.
pub const VSOCK_CONFIG_SIZE: u16 = 8;
/// One vector per queue plus one for config change.
pub const VSOCK_MSIX_VECTORS: u16 = VSOCK_NUM_QUEUES as u16 + 1;

pub struct VirtioVsock {
    shared: Arc<VsockShared>,
    accept: Mutex<Option<JoinHandle<()>>>,
    negotiated_features: Mutex<u64>,
    indicator: Indicator,
}

impl VirtioVsock {
    /// Bind the host socket and start accepting clients.
    ///
    /// Returns by value: `VirtioPciDevice::new` takes the backend by
    /// value, so clone [`interrupt`](Self::interrupt) before the move.
    pub fn new(
        socket_path: &Path,
        guest_cid: u64,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            guest_cid >= CID_GUEST_MIN,
            "virtio-vsock cid must be {CID_GUEST_MIN} or above, got {guest_cid}"
        );
        // 0600 socket in a 0700 directory, same policy as the console
        // and the control socket. Not a bare UnixListener::bind.
        let listener = bind_restricted(socket_path, SocketPolicy::default())
            .with_context(|| {
                format!("bind virtio-vsock socket {}", socket_path.display())
            })?;

        // Zero registered workers: the accept and reader threads block
        // on sockets, outside the gate. The gate's job here is
        // `is_paused`, so a paused device stops touching guest memory.
        let shared = Arc::new(VsockShared {
            mux: VsockMux::new(guest_cid),
            physmap,
            pending_rx: Mutex::new(VecDeque::new()),
            rx_completion: Mutex::new(None),
            delivery: Mutex::new(()),
            interrupt: Arc::new(IntrSlot::new()),
            sockets: Mutex::new(HashMap::new()),
            control: ControlSlot::default(),
            socket_path: socket_path.to_path_buf(),
            readers: Mutex::new(Vec::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(QuiesceGate::new(0)),
            log,
        });

        let accept_shared = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("virtio-vsock-accept".into())
            .spawn(move || listener::accept_loop(accept_shared, listener))
            .context("spawn virtio-vsock accept thread")?;

        Ok(Self {
            shared,
            accept: Mutex::new(Some(accept)),
            negotiated_features: Mutex::new(0),
            indicator: Indicator::new(),
        })
    }

    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.shared.interrupt.install(intr);
    }

    /// The control slot, for the binary to fill once the device registry
    /// exists. Clone this before moving the device into the transport.
    pub fn control_slot(&self) -> ControlSlot {
        self.shared.control.clone()
    }

    pub const fn config_size(&self) -> u16 {
        VSOCK_CONFIG_SIZE
    }

    pub fn socket_path(&self) -> &Path {
        &self.shared.socket_path
    }
}

impl VirtioDevice for VirtioVsock {
    fn device_features(&self) -> u64 {
        // Ring features only. The transport adds VERSION_1 and strips
        // these two on the legacy path.
        crate::bits::VIRTIO_F_RING_EVENT_IDX
            | crate::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, features: u64) {
        // Store only: the modern transport calls this twice, once per
        // 32-bit half, with a partial value on the first call.
        *self.negotiated_features.lock().expect("features lock") = features;
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        // The whole config is one le64 guest_cid.
        let cid = self.shared.mux.guest_cid().to_le_bytes();
        crate::cfg_read_bytes(&cid, offset, len)
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        0 // not called: notify_queue is overridden
    }

    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(queue_idx as usize) else {
            return false;
        };
        match queue_idx {
            VSOCK_RX_QUEUE => {
                self.stash_rx(queue, physmap);
                // The completion handler raises the interrupt later.
                false
            }
            VSOCK_TX_QUEUE => self.drain_tx(queue, physmap),
            VSOCK_EVENT_QUEUE => {
                // Event buffers are accepted and never used: the only
                // event defined is transport reset, which needs a
                // migration this binary does not have.
                false
            }
            _ => false,
        }
    }

    /// The handler snapshots the ring it was built on, so a ring the
    /// driver programs again needs a new one, and the chains stashed
    /// from the old ring go with it.
    fn queue_addr_set(&self, queue_idx: u16, _queue: &VirtQueue) {
        if queue_idx != VSOCK_RX_QUEUE {
            return;
        }
        *self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock") = None;
        self.shared
            .pending_rx
            .lock()
            .expect("pending rx lock")
            .clear();
    }

    fn reset(&self) {
        *self.negotiated_features.lock().expect("features lock") = 0;
        *self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock") = None;
        // Chains posted before the reset are dropped, not completed:
        // the guest abandons its rings across a reset and reposts.
        self.shared
            .pending_rx
            .lock()
            .expect("pending rx lock")
            .clear();
    }
}

#[cfg(test)]
mod tests;
