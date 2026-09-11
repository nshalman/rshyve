// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One vsock connection: its state and its flow-control credit.
//!
//! Deliberately free of I/O. Everything here is a pure function of the
//! packets seen and the bytes moved, so the state machine can be tested
//! without a guest or a socket. `mux.rs` owns the host sockets and
//! drives this.

use super::packet::{
    VsockHdr, OP_CREDIT_UPDATE, OP_RESPONSE, OP_RST, OP_RW, OP_SHUTDOWN,
    SHUTDOWN_RCV, SHUTDOWN_SEND, TYPE_STREAM,
};

/// Receive window advertised to the guest, per connection.
pub const DEFAULT_BUF_ALLOC: u32 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The host asked the guest to connect; awaiting RESPONSE.
    Connecting,
    Established,
    /// The host will send no more, the guest may still send.
    LocalSendClosed,
    /// The guest will send no more, the host may still send.
    PeerSendClosed,
    /// Dead. The device drops it and forgets the key.
    Closed,
}

#[derive(Debug)]
pub struct VsockConn {
    state: State,
    /// Size of the guest's receive buffer, as the guest reports it.
    peer_buf_alloc: u32,
    /// Bytes the guest says it has consumed from our stream.
    peer_fwd_cnt: u32,
    /// Bytes we have sent to the guest.
    tx_cnt: u32,
    /// Our receive window, advertised to the guest.
    buf_alloc: u32,
    /// Bytes we have forwarded from the guest to the host socket.
    fwd_cnt: u32,
}

impl VsockConn {
    pub fn new(state: State) -> Self {
        Self {
            state,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            buf_alloc: DEFAULT_BUF_ALLOC,
            fwd_cnt: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// Bytes the host may still send before the guest's buffer is full.
    ///
    /// The counters are `u32` and wrap, so the in-flight figure is a
    /// wrapping subtraction. A guest that reports `fwd_cnt` ahead of what
    /// we ever sent makes the in-flight figure enormous rather than
    /// negative, and this returns zero: credit fails closed, so a lying
    /// guest can stall its own connection but never make us overrun its
    /// buffer.
    pub fn peer_credit(&self) -> u32 {
        let in_flight = self.tx_cnt.wrapping_sub(self.peer_fwd_cnt);
        self.peer_buf_alloc.saturating_sub(in_flight)
    }

    /// Take the credit fields off any packet from the guest. Every
    /// packet carries them, not just CREDIT_UPDATE.
    pub fn absorb_credit(&mut self, hdr: &VsockHdr) {
        self.peer_buf_alloc = hdr.buf_alloc;
        self.peer_fwd_cnt = hdr.fwd_cnt;
    }

    /// Record bytes handed to the guest.
    pub fn record_sent(&mut self, n: u32) {
        self.tx_cnt = self.tx_cnt.wrapping_add(n);
    }

    /// Record bytes taken from the guest and written to the host socket.
    pub fn record_forwarded(&mut self, n: u32) {
        self.fwd_cnt = self.fwd_cnt.wrapping_add(n);
    }

    /// Credit fields to stamp on a packet heading to the guest.
    pub fn our_credit(&self) -> (u32, u32) {
        (self.buf_alloc, self.fwd_cnt)
    }

    /// Apply a packet from the guest. Returns the op to reply with, if
    /// the guest is owed one.
    pub fn on_packet(&mut self, hdr: &VsockHdr) -> Option<u16> {
        self.absorb_credit(hdr);

        // Only streams exist here; anything else is rejected rather than
        // guessed at.
        if hdr.type_ != TYPE_STREAM {
            self.state = State::Closed;
            return Some(OP_RST);
        }

        match hdr.op {
            OP_RESPONSE => {
                if self.state == State::Connecting {
                    self.state = State::Established;
                    None
                } else {
                    // A RESPONSE on an established connection is a
                    // protocol error, not something to act on.
                    self.state = State::Closed;
                    Some(OP_RST)
                }
            }
            OP_RST => {
                self.state = State::Closed;
                None
            }
            OP_SHUTDOWN => {
                // Both directions closed means the connection is over.
                // Reply with RST so the guest may reuse the port at once,
                // which is what the Linux driver expects.
                let both = SHUTDOWN_RCV | SHUTDOWN_SEND;
                if hdr.flags & both == both {
                    self.state = State::Closed;
                    return Some(OP_RST);
                }
                if hdr.flags & SHUTDOWN_SEND != 0 {
                    self.state = match self.state {
                        State::LocalSendClosed => State::Closed,
                        _ => State::PeerSendClosed,
                    };
                }
                if self.state == State::Closed {
                    Some(OP_RST)
                } else {
                    None
                }
            }
            // RW payload is the caller's business, and CREDIT_UPDATE
            // needs nothing beyond the credit absorbed above.
            OP_RW | OP_CREDIT_UPDATE => None,
            _ => None,
        }
    }

    /// The host end went away: tell the guest to stop sending.
    ///
    /// A connection the guest has not answered yet closes outright.
    /// There is no half open state to keep: the guest was offered the
    /// connection and never took it, and holding the entry until it
    /// replies lets a peer that connects and hangs up fill the table.
    pub fn close_local_send(&mut self) {
        self.state = match self.state {
            State::Connecting => State::Closed,
            State::PeerSendClosed => State::Closed,
            State::Closed => State::Closed,
            _ => State::LocalSendClosed,
        };
    }

    /// Whether the guest may still send payload to the host.
    pub fn accepts_guest_data(&self) -> bool {
        matches!(self.state, State::Established | State::LocalSendClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vsock::packet::{CID_HOST, OP_REQUEST};

    fn hdr(op: u16) -> VsockHdr {
        VsockHdr {
            src_cid: 3,
            dst_cid: CID_HOST,
            src_port: 1024,
            dst_port: 5000,
            len: 0,
            type_: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 4096,
            fwd_cnt: 0,
        }
    }

    #[test]
    fn response_establishes_a_pending_connection() {
        let mut c = VsockConn::new(State::Connecting);
        assert_eq!(c.on_packet(&hdr(OP_RESPONSE)), None);
        assert_eq!(c.state(), State::Established);
    }

    #[test]
    fn a_second_response_is_a_protocol_error() {
        let mut c = VsockConn::new(State::Established);
        assert_eq!(c.on_packet(&hdr(OP_RESPONSE)), Some(OP_RST));
        assert!(c.is_closed());
    }

    #[test]
    fn rst_closes_without_a_reply() {
        let mut c = VsockConn::new(State::Established);
        assert_eq!(c.on_packet(&hdr(OP_RST)), None);
        assert!(c.is_closed());
    }

    #[test]
    fn a_non_stream_connection_is_refused() {
        let mut c = VsockConn::new(State::Established);
        let mut h = hdr(OP_REQUEST);
        h.type_ = 2; // SEQPACKET, which is not implemented
        assert_eq!(c.on_packet(&h), Some(OP_RST));
        assert!(c.is_closed());
    }

    #[test]
    fn full_shutdown_closes_and_replies_rst() {
        let mut c = VsockConn::new(State::Established);
        let mut h = hdr(OP_SHUTDOWN);
        h.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
        assert_eq!(c.on_packet(&h), Some(OP_RST));
        assert!(c.is_closed());
    }

    #[test]
    fn half_shutdown_keeps_the_other_direction_open() {
        let mut c = VsockConn::new(State::Established);
        let mut h = hdr(OP_SHUTDOWN);
        h.flags = SHUTDOWN_SEND;
        assert_eq!(c.on_packet(&h), None);
        assert_eq!(c.state(), State::PeerSendClosed);
        assert!(!c.is_closed());
    }

    #[test]
    fn both_halves_closing_ends_the_connection() {
        let mut c = VsockConn::new(State::Established);
        c.close_local_send();
        assert_eq!(c.state(), State::LocalSendClosed);
        let mut h = hdr(OP_SHUTDOWN);
        h.flags = SHUTDOWN_SEND;
        assert_eq!(c.on_packet(&h), Some(OP_RST));
        assert!(c.is_closed());
    }

    #[test]
    fn credit_tracks_what_the_guest_has_consumed() {
        let mut c = VsockConn::new(State::Established);
        c.absorb_credit(&hdr(OP_CREDIT_UPDATE));
        assert_eq!(c.peer_credit(), 4096);

        c.record_sent(1000);
        assert_eq!(c.peer_credit(), 3096);

        // The guest reports consuming 600 of them.
        let mut h = hdr(OP_CREDIT_UPDATE);
        h.fwd_cnt = 600;
        c.on_packet(&h);
        assert_eq!(c.peer_credit(), 4096 - 400);
    }

    #[test]
    fn a_full_peer_buffer_yields_no_credit() {
        let mut c = VsockConn::new(State::Established);
        c.absorb_credit(&hdr(OP_CREDIT_UPDATE));
        c.record_sent(4096);
        assert_eq!(c.peer_credit(), 0);
    }

    /// The counters are u32 and wrap by design. Credit has to stay
    /// correct across the wrap, or a long-lived connection stalls or,
    /// worse, is granted a window it does not have.
    #[test]
    fn credit_survives_counter_wrap() {
        let mut c = VsockConn::new(State::Established);
        let mut h = hdr(OP_CREDIT_UPDATE);
        h.buf_alloc = 4096;
        h.fwd_cnt = u32::MAX - 100;
        c.on_packet(&h);
        c.tx_cnt = u32::MAX - 100;

        c.record_sent(200); // wraps past u32::MAX
        assert_eq!(c.tx_cnt, 99);
        assert_eq!(c.peer_credit(), 4096 - 200);
    }

    /// A guest that claims to have consumed more than we ever sent must
    /// not be handed a larger window than it advertised.
    #[test]
    fn a_lying_fwd_cnt_fails_closed() {
        let mut c = VsockConn::new(State::Established);
        let mut h = hdr(OP_CREDIT_UPDATE);
        h.buf_alloc = 4096;
        h.fwd_cnt = 1_000_000; // we have sent nothing
        c.on_packet(&h);
        assert_eq!(c.peer_credit(), 0, "credit must fail closed, not wrap up");
    }

    #[test]
    fn our_advertised_window_tracks_what_we_forwarded() {
        let mut c = VsockConn::new(State::Established);
        assert_eq!(c.our_credit(), (DEFAULT_BUF_ALLOC, 0));
        c.record_forwarded(512);
        assert_eq!(c.our_credit(), (DEFAULT_BUF_ALLOC, 512));
    }

    #[test]
    fn data_is_refused_once_the_guest_has_shut_down_sending() {
        let mut c = VsockConn::new(State::Established);
        assert!(c.accepts_guest_data());
        let mut h = hdr(OP_SHUTDOWN);
        h.flags = SHUTDOWN_SEND;
        c.on_packet(&h);
        assert!(!c.accepts_guest_data());
    }
}
