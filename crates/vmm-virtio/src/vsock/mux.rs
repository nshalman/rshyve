// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The connection table and the guest-to-host dispatch.
//!
//! One device serves one guest, so a connection is named by its two
//! ports alone. Packets bound for the guest wait here until the guest
//! posts an RX chain to put them in, the same way the console buffers
//! host input.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use super::conn::{State, VsockConn};
use super::packet::{
    ConnKey, VsockHdr, CID_HOST, OP_REQUEST, OP_RESPONSE, OP_RST, OP_RW,
    OP_SHUTDOWN, SHUTDOWN_RCV, SHUTDOWN_SEND, TYPE_STREAM,
};

/// Bytes of guest-bound payload buffered before the device stops
/// reading host sockets. Without a cap a fast host writer grows VMM
/// memory without limit while the guest is not reading.
pub const RX_BACKLOG_MAX: usize = 1024 * 1024;

/// Live connections one device holds. Both ends open them: a host peer
/// that connects, and a guest that sends REQUEST. Each one costs a
/// table entry, a host socket and a reader thread, so without a cap
/// either end can exhaust the VMM. cloud-hypervisor caps its muxer at
/// 1023 for the same reason.
pub const MAX_CONNS: usize = 1024;

/// Packets queued for the guest, of any kind.
///
/// [`RX_BACKLOG_MAX`] bounds payload bytes, but a control packet
/// carries none and is admitted anyway, so it needs a separate bound: a
/// guest that never posts an RX chain can make the device reply to
/// packets far faster than the queue drains. Deep enough for several
/// control packets per live connection.
pub const RX_PACKETS_MAX: usize = MAX_CONNS * 4;

/// Host ports handed to guest-initiated connections start here, above
/// anything a caller is likely to have named.
const EPHEMERAL_PORT_BASE: u32 = 49152;

/// A packet waiting for an RX chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxPacket {
    pub hdr: VsockHdr,
    pub payload: Vec<u8>,
}

impl RxPacket {
    /// A header-only packet, which is every control packet.
    pub fn control(hdr: VsockHdr) -> Self {
        Self {
            hdr,
            payload: Vec::new(),
        }
    }

    /// Total wire size: header plus payload.
    pub fn wire_len(&self) -> usize {
        super::packet::HDR_SIZE + self.payload.len()
    }
}

/// What the device should do with a packet the guest sent.
#[derive(Debug, PartialEq, Eq)]
pub enum GuestAction {
    /// Nothing further.
    None,
    /// Write this payload to the connection's host socket.
    Deliver(ConnKey, Vec<u8>),
    /// Close the host socket for this connection and forget it.
    Close(ConnKey),
    /// A guest-initiated connection to a host port.
    Connect(ConnKey, u32),
}

/// Packets waiting for an RX chain, and the payload bytes they hold.
///
/// One lock, not two: the queue, its length cap and its byte count are
/// one invariant, and both caps have to be tested and applied without
/// letting go in between.
#[derive(Default)]
struct RxQueue {
    q: VecDeque<RxPacket>,
    bytes: usize,
}

pub struct VsockMux {
    guest_cid: u64,
    conns: Mutex<HashMap<ConnKey, VsockConn>>,
    rx: Mutex<RxQueue>,
    next_host_port: AtomicU32,
}

impl VsockMux {
    pub fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            conns: Mutex::new(HashMap::new()),
            rx: Mutex::new(RxQueue::default()),
            next_host_port: AtomicU32::new(EPHEMERAL_PORT_BASE),
        }
    }

    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// A host port for a connection the guest did not name.
    pub fn alloc_host_port(&self) -> u32 {
        self.next_host_port.fetch_add(1, Ordering::Relaxed)
    }

    /// Queue a packet for the guest. Returns false when the backlog is
    /// full, which tells the caller to stop reading its host socket.
    pub fn push_rx(&self, pkt: RxPacket) -> bool {
        let mut rx = self.rx.lock().expect("rx lock");
        // The packet cap holds for every kind of packet. A queue this
        // deep means the guest has stopped draining, so its connections
        // are wedged already, and dropping one more packet is what
        // keeps the VMM standing. cloud-hypervisor drops an RST on a
        // full queue for the same reason.
        if rx.q.len() >= RX_PACKETS_MAX {
            return false;
        }
        // Below that, a control packet is always admitted: dropping a
        // RST or a RESPONSE wedges the connection it belongs to, and
        // they are small enough that the byte cap stays meaningful.
        if !pkt.payload.is_empty()
            && rx.bytes.saturating_add(pkt.payload.len()) > RX_BACKLOG_MAX
        {
            return false;
        }
        rx.bytes = rx.bytes.saturating_add(pkt.payload.len());
        rx.q.push_back(pkt);
        true
    }

    /// Next packet for the guest, if any.
    pub fn pop_rx(&self) -> Option<RxPacket> {
        let mut rx = self.rx.lock().expect("rx lock");
        let pkt = rx.q.pop_front()?;
        rx.bytes = rx.bytes.saturating_sub(pkt.payload.len());
        Some(pkt)
    }

    /// Put a packet back at the head of the queue, keeping stream
    /// order. Used when a chain was too small to take all of it.
    ///
    /// Not subject to either cap: this packet was just popped, so it
    /// returns the queue to a size it already held.
    pub fn requeue_rx_front(&self, pkt: RxPacket) {
        let mut rx = self.rx.lock().expect("rx lock");
        rx.bytes = rx.bytes.saturating_add(pkt.payload.len());
        rx.q.push_front(pkt);
    }

    pub fn rx_is_empty(&self) -> bool {
        self.rx.lock().expect("rx lock").q.is_empty()
    }

    /// Register a host-initiated connection and queue its REQUEST.
    ///
    /// Returns false when the table is at [`MAX_CONNS`]. Nothing is
    /// registered and the guest is not told, so the caller has to close
    /// the peer's socket: no answer will ever come for it.
    ///
    /// A caller that must write to the host socket first takes the two
    /// steps apart: [`Self::reserve_host_connect`], then its own write,
    /// then [`Self::announce_host_connect`].
    pub fn start_host_connect(&self, key: ConnKey) -> bool {
        if !self.reserve_host_connect(key) {
            return false;
        }
        self.announce_host_connect(key);
        true
    }

    /// Take a slot for a host-initiated connection, without telling the
    /// guest about it.
    ///
    /// The guest cannot reach the connection until
    /// [`Self::announce_host_connect`] queues its REQUEST, so the caller
    /// owns the host socket alone until then. Returns false when the
    /// table is at [`MAX_CONNS`].
    pub fn reserve_host_connect(&self, key: ConnKey) -> bool {
        let mut conns = self.conns.lock().expect("conns lock");
        // Replacing a key the table already holds does not grow it.
        if conns.len() >= MAX_CONNS && !conns.contains_key(&key) {
            return false;
        }
        conns.insert(key, VsockConn::new(State::Connecting));
        true
    }

    /// Tell the guest about a slot taken by
    /// [`Self::reserve_host_connect`].
    ///
    /// From this point any thread's `deliver_rx` may hand the REQUEST to
    /// the guest, so the caller must have finished with the host socket.
    pub fn announce_host_connect(&self, key: ConnKey) {
        let mut hdr = self.hdr_for(key, OP_REQUEST);
        self.stamp_credit(key, &mut hdr);
        self.push_rx(RxPacket::control(hdr));
    }

    /// Put this connection's receive window on a packet.
    ///
    /// Every header carries the credit fields (VIRTIO 1.3 5.10.6.3),
    /// and the guest takes them from whichever packet opens the
    /// connection. A zero window there tells the guest it may send
    /// nothing, and a guest that speaks first then waits for a window
    /// that only our own payload would ever open.
    fn stamp_credit(&self, key: ConnKey, hdr: &mut VsockHdr) {
        let conns = self.conns.lock().expect("conns lock");
        if let Some(conn) = conns.get(&key) {
            let (buf_alloc, fwd_cnt) = conn.our_credit();
            hdr.buf_alloc = buf_alloc;
            hdr.fwd_cnt = fwd_cnt;
        }
    }

    /// Header addressed from the host side of `key` to the guest.
    fn hdr_for(&self, key: ConnKey, op: u16) -> VsockHdr {
        VsockHdr {
            src_cid: CID_HOST,
            dst_cid: self.guest_cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            len: 0,
            type_: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        }
    }

    /// Queue payload read from a host socket, as much of it as the
    /// guest's credit and the backlog allow, and take what went out of
    /// `data`.
    ///
    /// The guest publishes a receive window per connection and drops
    /// whatever arrives past it, so bytes sent over the window are not
    /// slow, they are lost, and the guest reads a stream with a hole in
    /// it. The window is [`VsockConn::peer_credit`], which fails closed.
    ///
    /// `data` is the caller's retry buffer: what goes is removed from
    /// it, so a caller that offers the same buffer until it is empty
    /// sends every byte exactly once. Returning a count to subtract, or
    /// a bool meaning all or nothing, puts that on every caller, and a
    /// caller that gets it wrong resends bytes the guest already has.
    ///
    /// Takes the connection lock and then the queue lock. Nothing here
    /// takes them the other way round.
    pub fn queue_host_data(&self, key: ConnKey, data: &mut Vec<u8>) {
        let mut conns = self.conns.lock().expect("conns lock");
        let Some(conn) = conns.get_mut(&key) else {
            // Nothing knows this connection, so no window will ever open
            // for it. Keeping the bytes would park the caller for the
            // life of the VM.
            data.clear();
            return;
        };
        let take = (conn.peer_credit() as usize).min(data.len());
        if take == 0 {
            // The guest's window is shut. It opens again on the next
            // packet from the guest: every one carries fresh credit.
            return;
        }
        let (buf_alloc, fwd_cnt) = conn.our_credit();
        let mut hdr = self.hdr_for(key, OP_RW);
        hdr.len = take as u32;
        hdr.buf_alloc = buf_alloc;
        hdr.fwd_cnt = fwd_cnt;
        if !self.push_rx(RxPacket {
            hdr,
            payload: data[..take].to_vec(),
        }) {
            // The backlog is full, so nothing was queued: spend no
            // credit and take nothing.
            return;
        }
        // Only bytes really on their way count against the window. A
        // packet the backlog refused would otherwise shut the window on
        // bytes the guest never saw.
        conn.record_sent(take as u32);
        drop(conns);
        data.drain(..take);
    }

    /// Tell the guest the host end has gone.
    pub fn queue_host_close(&self, key: ConnKey) {
        let mut conns = self.conns.lock().expect("conns lock");
        let Some(conn) = conns.get_mut(&key) else {
            return;
        };
        conn.close_local_send();
        let closed = conn.is_closed();
        drop(conns);

        let mut hdr = self.hdr_for(key, OP_SHUTDOWN);
        hdr.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
        self.push_rx(RxPacket::control(hdr));
        if closed {
            self.forget(key);
        }
    }

    pub fn forget(&self, key: ConnKey) {
        self.conns.lock().expect("conns lock").remove(&key);
    }

    pub fn conn_count(&self) -> usize {
        self.conns.lock().expect("conns lock").len()
    }

    /// Apply one packet from the guest, queueing any reply it is owed.
    pub fn on_guest_packet(
        &self,
        hdr: &VsockHdr,
        payload: Vec<u8>,
    ) -> GuestAction {
        // The guest names the connection from its own side.
        let key = ConnKey {
            guest_port: hdr.src_port,
            host_port: hdr.dst_port,
        };

        // A guest packet addressed to another CID is not ours to route.
        if hdr.dst_cid != CID_HOST {
            self.push_rx(RxPacket::control(hdr.reply(OP_RST)));
            return GuestAction::None;
        }

        let mut conns = self.conns.lock().expect("conns lock");

        // A REQUEST opens a connection the table does not have yet.
        if hdr.op == OP_REQUEST {
            // Refused for one of two reasons: the guest already has this
            // connection, or it has as many as it may hold. Each one
            // costs the device a host socket and a reader thread.
            if conns.contains_key(&key) || conns.len() >= MAX_CONNS {
                drop(conns);
                self.push_rx(RxPacket::control(hdr.reply(OP_RST)));
                return GuestAction::None;
            }
            let mut conn = VsockConn::new(State::Established);
            conn.absorb_credit(hdr);
            conns.insert(key, conn);
            drop(conns);
            return GuestAction::Connect(key, hdr.dst_port);
        }

        let Some(conn) = conns.get_mut(&key) else {
            // Nothing knows this connection. RST rather than silence, so
            // the guest stops retrying.
            drop(conns);
            if hdr.op != OP_RST {
                self.push_rx(RxPacket::control(hdr.reply(OP_RST)));
            }
            return GuestAction::None;
        };

        let reply = conn.on_packet(hdr);
        let closed = conn.is_closed();
        let accepts = conn.accepts_guest_data();
        if hdr.op == OP_RW {
            conn.record_forwarded(payload.len() as u32);
        }
        drop(conns);

        if let Some(op) = reply {
            self.push_rx(RxPacket::control(hdr.reply(op)));
        }
        if closed {
            self.forget(key);
            return GuestAction::Close(key);
        }
        if hdr.op == OP_RW && accepts && !payload.is_empty() {
            return GuestAction::Deliver(key, payload);
        }
        GuestAction::None
    }

    /// Accept a guest-initiated connection the host side agreed to.
    pub fn accept_guest_connect(&self, key: ConnKey) {
        if !self.conns.lock().expect("conns lock").contains_key(&key) {
            return;
        }
        let mut hdr = self.hdr_for(key, OP_RESPONSE);
        self.stamp_credit(key, &mut hdr);
        self.push_rx(RxPacket::control(hdr));
    }

    /// Refuse a guest-initiated connection the host side could not make.
    pub fn refuse_guest_connect(&self, key: ConnKey) {
        self.forget(key);
        self.push_rx(RxPacket::control(self.hdr_for(key, OP_RST)));
    }
}

#[cfg(test)]
mod tests;
