// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host to guest: queued packets into the receive ring.
//!
//! # Invariant
//!
//! One pairing pass at a time. Two threads pairing packets with chains
//! on their own can complete them in either order, the guest reads the
//! used ring in order, and RW carries a byte stream, so the stream
//! would be reassembled wrong. A split packet makes it worse: the tail
//! requeued at the head can be overtaken by the packet behind it.

use std::sync::Arc;

use slog::debug;
use vmm_core::mem::PhysMap;

use super::shared::VsockShared;
use super::{VirtioVsock, VSOCK_RX_QUEUE};
use crate::pci::intr::IntrSession;
use crate::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use crate::queue_drain_should_stop;
use crate::vsock::mux::RxPacket;
use crate::vsock::packet::HDR_SIZE;

impl VsockShared {
    /// Move queued packets into posted RX chains and publish them.
    pub(super) fn deliver_rx(&self) {
        // A paused device must not touch guest memory. Packets stay
        // queued and go out on resume.
        if self.gate.is_paused() {
            return;
        }
        // The raise goes outside the permission that wrote the ring:
        // delivery is a call into the kernel and a reset must not wait
        // on it. The handler names its own session, so a raise the
        // reset overtook is refused by the transport.
        if let Some(completion) = self.pair_rx() {
            completion.signal();
        }
    }

    /// Pair every queued packet with a posted chain, returning the
    /// handler that owes the guest an interrupt.
    fn pair_rx(&self) -> Option<Arc<VirtioCompletion>> {
        let _delivery = self.delivery.lock().expect("delivery lock");
        let mut raise = None;
        loop {
            if self.mux.rx_is_empty() {
                return raise;
            }
            // Taken per packet, and the handler is read inside it: a
            // reset drains this section before it drops the handler, so
            // the one found here belongs to the driver being written
            // to.
            let Some(_session) = self.access.enter_current() else {
                return raise;
            };
            let completion = {
                let guard =
                    self.rx_completion.lock().expect("rx completion lock");
                match guard.as_ref() {
                    Some(c) => Arc::clone(c),
                    None => return raise,
                }
            };
            let next = {
                let mut pending =
                    self.pending_rx.lock().expect("pending rx lock");
                pending.pop_front()
            };
            let Some((head, bufs)) = next else {
                return raise;
            };
            let Some(pkt) = self.mux.pop_rx() else {
                // Nothing to send after all: give the chain back rather
                // than completing it empty.
                self.pending_rx
                    .lock()
                    .expect("pending rx lock")
                    .push_front((head, bufs));
                return raise;
            };

            let written = self.write_packet(&bufs, pkt);
            if completion.publish_batch(&[(head, written)]) {
                raise = Some(completion);
            }
        }
    }

    /// Write one packet into a chain, splitting it when the chain is too
    /// small. RW carries a byte stream, so a split is legitimate; the
    /// remainder goes back to the front of the queue in order.
    fn write_packet(&self, bufs: &[ChainBuf], mut pkt: RxPacket) -> u32 {
        let capacity: usize = bufs
            .iter()
            .filter_map(|b| match b {
                ChainBuf::Writable { len, .. } => Some(*len as usize),
                _ => None,
            })
            .sum();
        if capacity < HDR_SIZE {
            // Cannot even carry a header. Drop the chain, keep the
            // packet for a chain that can.
            self.mux.requeue_rx_front(pkt);
            return 0;
        }

        let room = capacity - HDR_SIZE;
        if pkt.payload.len() > room {
            let rest = pkt.payload.split_off(room);
            let mut tail = pkt.clone();
            tail.payload = rest;
            tail.hdr.len = tail.payload.len() as u32;
            self.mux.requeue_rx_front(tail);
        }
        pkt.hdr.len = pkt.payload.len() as u32;

        let mut blob = Vec::with_capacity(HDR_SIZE + pkt.payload.len());
        blob.extend_from_slice(&pkt.hdr.to_bytes());
        blob.extend_from_slice(&pkt.payload);

        let mut written = 0usize;
        for buf in bufs {
            let ChainBuf::Writable { addr, len } = buf else {
                continue;
            };
            let remaining = blob.len() - written;
            if remaining == 0 {
                break;
            }
            let want = (*len as usize).min(remaining);
            let Some(sub) = self.physmap.lookup(*addr, want) else {
                break;
            };
            if sub.write_bytes(&blob[written..written + want]).is_err() {
                break;
            }
            written += want;
        }
        written as u32
    }
}

impl VirtioVsock {
    /// Get or create the completion handler for the RX ring.
    ///
    /// The session is read and the handler is stored under one lock,
    /// and [`crate::VirtioDevice::reset`] drops the handler under the
    /// same lock after the transport has ended its session. So a
    /// handler this leaves behind names either the session that is
    /// ending, and is then dropped, or the one that follows.
    pub(super) fn ensure_rx_completion(
        &self,
        queue: &VirtQueue,
    ) -> Arc<VirtioCompletion> {
        let mut slot = self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock");
        if let Some(completion) = slot.as_ref() {
            return Arc::clone(completion);
        }
        let intr = Arc::clone(&self.shared.interrupt);
        let session = self.shared.interrupt.next_session(IntrSession::INITIAL);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.shared.physmap),
            session,
            move |session| {
                intr.raise(session, VSOCK_RX_QUEUE);
            },
        );
        // Force the first completions through EVENT_IDX suppression so
        // an idle guest always wakes on the first packet.
        completion.set_force_interrupt(32);
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Pop RX chains and hold them until a packet needs one.
    pub(super) fn stash_rx(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        let completion = self.ensure_rx_completion(queue);

        // A stashed chain is only completed once a packet lands in it,
        // so the stash needs a bound of its own: a guest can otherwise
        // publish one descriptor repeatedly and grow VMM memory.
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        loop {
            let before = queue.last_avail_idx();
            let mut refused: Vec<u16> = Vec::new();
            {
                let mut pending =
                    self.shared.pending_rx.lock().expect("pending rx lock");
                while popped < max_requests && pending.len() < max_requests {
                    let Some(head) = queue.pop_avail(physmap) else {
                        break;
                    };
                    popped += 1;
                    let Some(chain) = queue.collect_chain(physmap, head) else {
                        refused.push(head);
                        continue;
                    };
                    pending.push_back((head, chain));
                }
            }
            // `pop_avail` already took these heads out of the available
            // ring, so each one has to go back on the used ring. Losing
            // one costs the guest a descriptor for the life of the
            // device, and there is no abort path to recover it.
            if !refused.is_empty() {
                debug!(self.shared.log, "virtio-vsock refused rx chains";
                    "count" => refused.len());
                for head in refused {
                    completion.complete(head, 0);
                }
            }
            // Arm the kick before leaving, on every path out.
            //
            // Skipping this when the request cap is hit is only sound
            // for a drain that comes straight back, and this one does
            // not: the stash is the device's whole supply of chains,
            // and the only thing that refills it is the guest kicking.
            // Linux fills the ring the moment it probes, so the very
            // first pass hits the cap, and a device that left
            // `avail_event` unarmed there was never kicked again: it
            // spent the chains it had, then held every packet for a
            // guest that could not be told. New connections hang with
            // it, because a RESPONSE needs a chain like any other
            // packet.
            let hit_request_cap = popped >= max_requests;
            queue.update_used_event(physmap);
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
        }
        self.shared.deliver_rx();
    }
}
