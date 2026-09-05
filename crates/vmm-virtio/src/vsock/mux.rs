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
    pub fn start_host_connect(&self, key: ConnKey) -> bool {
        {
            let mut conns = self.conns.lock().expect("conns lock");
            // Replacing a key the table already holds does not grow it.
            if conns.len() >= MAX_CONNS && !conns.contains_key(&key) {
                return false;
            }
            conns.insert(key, VsockConn::new(State::Connecting));
        }
        self.push_rx(RxPacket::control(self.hdr_for(key, OP_REQUEST)));
        true
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

    /// Queue payload read from a host socket, stamped with this
    /// connection's current credit. Returns false when the backlog is
    /// full.
    pub fn queue_host_data(&self, key: ConnKey, data: Vec<u8>) -> bool {
        let mut conns = self.conns.lock().expect("conns lock");
        let Some(conn) = conns.get_mut(&key) else {
            return true;
        };
        let (buf_alloc, fwd_cnt) = conn.our_credit();
        conn.record_sent(data.len() as u32);
        drop(conns);

        let mut hdr = self.hdr_for(key, OP_RW);
        hdr.len = data.len() as u32;
        hdr.buf_alloc = buf_alloc;
        hdr.fwd_cnt = fwd_cnt;
        self.push_rx(RxPacket { hdr, payload: data })
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
        let (buf_alloc, fwd_cnt) = {
            let conns = self.conns.lock().expect("conns lock");
            match conns.get(&key) {
                Some(c) => c.our_credit(),
                None => return,
            }
        };
        let mut hdr = self.hdr_for(key, OP_RESPONSE);
        hdr.buf_alloc = buf_alloc;
        hdr.fwd_cnt = fwd_cnt;
        self.push_rx(RxPacket::control(hdr));
    }

    /// Refuse a guest-initiated connection the host side could not make.
    pub fn refuse_guest_connect(&self, key: ConnKey) {
        self.forget(key);
        self.push_rx(RxPacket::control(self.hdr_for(key, OP_RST)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_CID: u64 = 3;

    fn mux() -> VsockMux {
        VsockMux::new(GUEST_CID)
    }

    fn guest_hdr(op: u16, guest_port: u32, host_port: u32) -> VsockHdr {
        VsockHdr {
            src_cid: GUEST_CID,
            dst_cid: CID_HOST,
            src_port: guest_port,
            dst_port: host_port,
            len: 0,
            type_: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 64 * 1024,
            fwd_cnt: 0,
        }
    }

    fn drain(m: &VsockMux) -> Vec<RxPacket> {
        let mut out = Vec::new();
        while let Some(p) = m.pop_rx() {
            out.push(p);
        }
        out
    }

    #[test]
    fn a_host_connect_queues_a_request_to_the_guest() {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        m.start_host_connect(key);

        let pkts = drain(&m);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].hdr.op, OP_REQUEST);
        assert_eq!(pkts[0].hdr.src_cid, CID_HOST);
        assert_eq!(pkts[0].hdr.dst_cid, GUEST_CID);
        assert_eq!(pkts[0].hdr.dst_port, 1024);
        assert_eq!(m.conn_count(), 1);
    }

    #[test]
    fn the_guest_response_establishes_it() {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        m.start_host_connect(key);
        drain(&m);

        let act =
            m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);
        assert_eq!(act, GuestAction::None);
        assert!(drain(&m).is_empty(), "an accepted response needs no reply");
    }

    #[test]
    fn guest_payload_is_delivered_to_the_host_socket() {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        m.start_host_connect(key);
        m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);
        drain(&m);

        let mut h = guest_hdr(OP_RW, 1024, 50000);
        h.len = 5;
        let act = m.on_guest_packet(&h, b"hello".to_vec());
        assert_eq!(act, GuestAction::Deliver(key, b"hello".to_vec()));
    }

    #[test]
    fn a_guest_request_opens_a_connection_outward() {
        let m = mux();
        let act = m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
        assert_eq!(
            act,
            GuestAction::Connect(
                ConnKey {
                    guest_port: 1024,
                    host_port: 5000
                },
                5000
            )
        );
        assert_eq!(m.conn_count(), 1);
    }

    #[test]
    fn accepting_a_guest_connect_answers_with_response() {
        let m = mux();
        m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
        let key = ConnKey {
            guest_port: 1024,
            host_port: 5000,
        };
        m.accept_guest_connect(key);

        let pkts = drain(&m);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].hdr.op, OP_RESPONSE);
        assert_ne!(pkts[0].hdr.buf_alloc, 0, "must advertise a window");
    }

    #[test]
    fn refusing_a_guest_connect_answers_with_rst() {
        let m = mux();
        m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
        let key = ConnKey {
            guest_port: 1024,
            host_port: 5000,
        };
        m.refuse_guest_connect(key);

        let pkts = drain(&m);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].hdr.op, OP_RST);
        assert_eq!(m.conn_count(), 0);
    }

    /// A packet for a connection nobody knows must be answered, or the
    /// guest retries forever.
    #[test]
    fn an_unknown_connection_gets_rst() {
        let m = mux();
        let act = m.on_guest_packet(&guest_hdr(OP_RW, 7, 9), b"x".to_vec());
        assert_eq!(act, GuestAction::None);
        let pkts = drain(&m);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].hdr.op, OP_RST);
    }

    /// An RST for an unknown connection must not be answered with
    /// another RST, or two peers ping-pong forever.
    #[test]
    fn an_rst_for_an_unknown_connection_is_dropped() {
        let m = mux();
        m.on_guest_packet(&guest_hdr(OP_RST, 7, 9), vec![]);
        assert!(drain(&m).is_empty());
    }

    /// A guest may address any CID it likes. Only the host's own is
    /// routable here.
    #[test]
    fn a_packet_for_another_cid_is_refused() {
        let m = mux();
        let mut h = guest_hdr(OP_REQUEST, 1024, 5000);
        h.dst_cid = 99;
        let act = m.on_guest_packet(&h, vec![]);
        assert_eq!(act, GuestAction::None);
        assert_eq!(drain(&m)[0].hdr.op, OP_RST);
        assert_eq!(m.conn_count(), 0, "no connection for a foreign CID");
    }

    #[test]
    fn a_duplicate_request_is_refused_without_disturbing_the_original() {
        let m = mux();
        m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
        drain(&m);
        let act = m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
        assert_eq!(act, GuestAction::None);
        assert_eq!(drain(&m)[0].hdr.op, OP_RST);
        assert_eq!(m.conn_count(), 1);
    }

    #[test]
    fn shutdown_from_the_guest_closes_and_reports() {
        let m = mux();
        m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
        drain(&m);
        let key = ConnKey {
            guest_port: 1024,
            host_port: 5000,
        };

        let mut h = guest_hdr(OP_SHUTDOWN, 1024, 5000);
        h.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
        assert_eq!(m.on_guest_packet(&h, vec![]), GuestAction::Close(key));
        assert_eq!(m.conn_count(), 0);
    }

    /// The cap exists so a host writer cannot grow VMM memory without
    /// bound while the guest is not reading.
    #[test]
    fn the_rx_backlog_is_bounded() {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        m.start_host_connect(key);
        m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);

        let chunk = vec![0u8; 64 * 1024];
        let mut accepted = 0;
        for _ in 0..64 {
            if m.queue_host_data(key, chunk.clone()) {
                accepted += 1;
            }
        }
        assert!(accepted > 0, "some data should be accepted");
        assert!(accepted < 64, "the backlog must refuse eventually");
        assert!(accepted * chunk.len() <= RX_BACKLOG_MAX + chunk.len());
    }

    /// Dropping a control packet would wedge the connection it belongs
    /// to, so the cap must not apply to them.
    #[test]
    fn control_packets_bypass_the_backlog_cap() {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        m.start_host_connect(key);
        m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);
        while m.queue_host_data(key, vec![0u8; 64 * 1024]) {}

        assert!(
            m.push_rx(RxPacket::control(m.hdr_for(key, OP_RST))),
            "a control packet must still be admitted when full"
        );
    }

    #[test]
    fn popping_frees_backlog_room() {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        m.start_host_connect(key);
        m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);
        while m.queue_host_data(key, vec![0u8; 64 * 1024]) {}

        drain(&m);
        assert!(
            m.queue_host_data(key, vec![0u8; 64 * 1024]),
            "room must come back once the guest has taken the packets"
        );
    }

    /// Each connection costs a host socket and a reader thread, so a
    /// guest that opens them faster than it closes them must be stopped
    /// at a cap rather than left to exhaust the VMM.
    #[test]
    fn the_guest_cannot_grow_the_connection_table_without_bound() {
        let m = mux();
        for port in 0..(MAX_CONNS as u32 + 64) {
            m.on_guest_packet(&guest_hdr(OP_REQUEST, port, 5000), vec![]);
            drain(&m);
        }
        assert_eq!(m.conn_count(), MAX_CONNS);

        // The one over the cap is refused, and told so.
        let act =
            m.on_guest_packet(&guest_hdr(OP_REQUEST, 900_000, 5000), vec![]);
        assert_eq!(act, GuestAction::None);
        assert_eq!(drain(&m)[0].hdr.op, OP_RST);
    }

    /// The same cap from the host side, where a peer that connects over
    /// and over is the one driving it.
    #[test]
    fn host_connects_stop_at_the_connection_cap() {
        let m = mux();
        for port in 0..MAX_CONNS as u32 {
            assert!(m.start_host_connect(ConnKey {
                guest_port: 1024,
                host_port: 50_000 + port,
            }));
            drain(&m);
        }

        assert!(!m.start_host_connect(ConnKey {
            guest_port: 1024,
            host_port: 40_000,
        }));
        assert_eq!(m.conn_count(), MAX_CONNS);
        assert!(
            drain(&m).is_empty(),
            "a refused connect must not ask the guest to open one"
        );
    }

    /// A control packet carries no payload, so the byte cap never sees
    /// it. A guest that stops draining must still not be able to grow
    /// the queue by sending packets the device has to answer.
    #[test]
    fn control_packets_cannot_grow_the_rx_queue_without_bound() {
        let m = mux();
        for port in 0..(RX_PACKETS_MAX as u32 * 2) {
            m.on_guest_packet(&guest_hdr(OP_RW, port, 9), vec![]);
        }
        assert_eq!(m.conn_count(), 0, "none of these open a connection");
        assert_eq!(drain(&m).len(), RX_PACKETS_MAX);
    }

    #[test]
    fn host_ports_are_unique() {
        let m = mux();
        let a = m.alloc_host_port();
        let b = m.alloc_host_port();
        assert_ne!(a, b);
        assert!(a >= EPHEMERAL_PORT_BASE);
    }
}
