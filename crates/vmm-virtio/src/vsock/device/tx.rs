// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest to host: the transmit ring into the host sockets.
//!
//! # Invariant
//!
//! No length the guest wrote is trusted for a size. One chain builds at
//! most [`MAX_TX_PAYLOAD`] plus a header, and the header's own length
//! counts only as far as the bytes the guest really supplied. A guest
//! that claims more than it sent therefore reads back nothing it did
//! not put there, and cannot make the device allocate without bound.

use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use slog::{debug, warn};
use vmm_core::mem::PhysMap;

use super::listener::reader_loop;
use super::VirtioVsock;
use crate::queue::{ChainBuf, VirtQueue};
use crate::queue_drain_should_stop;
use crate::vsock::mux::GuestAction;
use crate::vsock::packet::{ConnKey, VsockHdr, HDR_SIZE};

/// Largest payload accepted from one guest TX chain, so a length the
/// guest picked never becomes an unbounded allocation.
const MAX_TX_PAYLOAD: usize = 64 * 1024;

impl VirtioVsock {
    /// Consume guest packets from the transmit queue.
    pub(super) fn drain_tx(
        &self,
        queue: &mut VirtQueue,
        physmap: &PhysMap,
    ) -> bool {
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        let mut used = false;
        loop {
            let before = queue.last_avail_idx();
            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                if let Some(chain) = queue.collect_chain(physmap, head) {
                    self.handle_tx_chain(&chain, physmap);
                }
                // The chain is device-readable only, so nothing was
                // written back into it.
                queue.push_used(physmap, head, 0);
                used = true;
            }
            // Arm the kick on every path out, as the RX stash does:
            // nothing returns to this ring without a guest kick, so a
            // cap exit that left `avail_event` unarmed would suppress
            // the kick the device needs to see the rest.
            let hit_request_cap = popped >= max_requests;
            queue.update_used_event(physmap);
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
        }
        if used {
            self.shared.deliver_rx();
        }
        used
    }

    /// Read one guest packet out of a chain and act on it.
    fn handle_tx_chain(&self, bufs: &[ChainBuf], physmap: &PhysMap) {
        let mut blob = Vec::new();
        for buf in bufs {
            let ChainBuf::Readable { addr, len } = buf else {
                continue;
            };
            // A length the guest picked must never become an unbounded
            // allocation, so the whole packet is capped.
            const CAP: usize = HDR_SIZE + MAX_TX_PAYLOAD;
            let want = (*len as usize).min(CAP.saturating_sub(blob.len()));
            if want == 0 {
                break;
            }
            let Some(sub) = physmap.lookup(*addr, want) else {
                return;
            };
            let mut chunk = vec![0u8; want];
            if sub.read_bytes(&mut chunk).is_err() {
                return;
            }
            blob.extend_from_slice(&chunk);
        }

        let Some(hdr) = VsockHdr::from_bytes(&blob) else {
            return;
        };
        // Trust the header's length only as far as the bytes actually
        // present: a guest may claim more than it supplied.
        let available = blob.len() - HDR_SIZE;
        let take = (hdr.len as usize).min(available).min(MAX_TX_PAYLOAD);
        let payload = blob[HDR_SIZE..HDR_SIZE + take].to_vec();

        match self.shared.mux.on_guest_packet(&hdr, payload) {
            GuestAction::None => {}
            GuestAction::Deliver(key, data) => {
                self.shared.write_host(key, &data)
            }
            GuestAction::Close(key) => self.shared.drop_socket(key),
            GuestAction::Connect(key, port) => self.connect_out(key, port),
        }
    }

    /// A guest-initiated connection: dial `<socket>_<port>` on the host.
    fn connect_out(&self, key: ConnKey, port: u32) {
        if !crate::vsock::guest_connects_allowed() {
            if !self.shared.warned_no_connects.swap(true, Ordering::AcqRel) {
                warn!(
                    self.shared.log,
                    "virtio-vsock refuses guest connects \
                    under the privilege lock"
                );
            }
            self.shared.mux.refuse_guest_connect(key);
            return;
        }
        // The reader table bounds the threads this device holds, and it
        // is one table: a guest that opened connections outside it
        // doubled the ceiling every other path is held to. Reserved
        // before the dial, so nothing is opened that cannot be served.
        let Some(slot) = self.shared.reserve_reader() else {
            debug!(self.shared.log, "virtio-vsock refused a guest connect";
                "reason" => "reader table full");
            self.shared.mux.refuse_guest_connect(key);
            return;
        };
        let path = format!("{}_{}", self.shared.socket_path.display(), port);
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(error) => {
                // Guest reachable and unbounded: a guest dialling a
                // closed port in a loop reaches this once per packet.
                debug!(self.shared.log, "virtio-vsock guest connect refused";
                    "path" => &path, "error" => %error);
                self.shared.mux.refuse_guest_connect(key);
                return;
            }
        };
        let socket = self.shared.register_socket(key, stream);
        self.shared.mux.accept_guest_connect(key);

        let shared = Arc::clone(&self.shared);
        let reader = Arc::clone(&socket);
        match std::thread::Builder::new()
            .name("vsock-reader".into())
            .spawn(move || reader_loop(shared, key, reader))
        {
            Ok(handle) => slot.fill(handle),
            Err(error) => {
                // Nothing would read this socket, and nothing would
                // tell the guest the host end had gone, so the
                // connection is undone rather than left accepted.
                warn!(self.shared.log,
                    "virtio-vsock reader thread not started";
                    "error" => %error);
                self.shared.release_socket(key, &socket);
                self.shared.mux.refuse_guest_connect(key);
            }
        }
    }
}
