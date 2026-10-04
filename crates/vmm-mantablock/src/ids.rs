// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Request ids and the watermark of one queue (spec 37 §5).
//!
//! `op_id = generation << 64 | queue << 48 | seq`. The sequence starts at
//! 0 for each queue in each generation and never repeats; the engine and
//! the log both take a repeated op id as a replay.

use std::collections::BTreeSet;

use manta_block_ring::OpId;

/// Sequences of one ring queue.
#[derive(Debug)]
pub(crate) struct Seqs {
    generation: u64,
    queue: u16,
    next: u64,
    /// Sequences sent and not yet completed.
    open: BTreeSet<u64>,
}

/// The queue used up its 48-bit sequence space. At 10 million requests a
/// second that takes 325 days of one attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exhausted;

impl Seqs {
    pub(crate) fn new(generation: u64, queue: u16) -> Self {
        Self {
            generation,
            queue,
            next: 0,
            open: BTreeSet::new(),
        }
    }

    /// The id for the next request, and the watermark to send with it:
    /// the lowest sequence this queue still holds without a completion,
    /// this one included.
    pub(crate) fn open(&mut self) -> Result<(OpId, u64), Exhausted> {
        let seq = self.next;
        if seq > OpId::MAX_SEQ {
            return Err(Exhausted);
        }
        self.next += 1;
        self.open.insert(seq);
        let watermark = self.open.first().copied().unwrap_or(seq);
        let id = OpId {
            generation: self.generation,
            queue: self.queue,
            seq,
        };
        Ok((id, watermark))
    }

    /// The request with `seq` has its completion.
    pub(crate) fn close(&mut self, seq: u64) {
        self.open.remove(&seq);
    }

    #[cfg(test)]
    pub(crate) fn skip_to(&mut self, seq: u64) {
        self.next = seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_id_carries_generation_queue_and_sequence() {
        let mut s = Seqs::new(0xabcd, 3);
        let (id, _) = s.open().unwrap();
        let (id2, _) = s.open().unwrap();
        assert_eq!(id.to_u128(), (0xabcd_u128 << 64) | (3 << 48));
        assert_eq!(id2.to_u128(), (0xabcd_u128 << 64) | (3 << 48) | 1);
        assert_eq!(OpId::from_u128(id2.to_u128()), id2);
    }

    #[test]
    fn the_watermark_is_the_oldest_open_sequence() {
        let mut s = Seqs::new(1, 0);
        let (a, w) = s.open().unwrap();
        assert_eq!(w, a.seq);
        let (b, w) = s.open().unwrap();
        assert_eq!(w, a.seq);
        let (c, _) = s.open().unwrap();
        // Out of order, as the engine may complete.
        s.close(b.seq);
        let (_, w) = s.open().unwrap();
        assert_eq!(w, a.seq);
        s.close(a.seq);
        let (d, w) = s.open().unwrap();
        assert_eq!(w, c.seq);
        s.close(c.seq);
        s.close(d.seq);
        let (e, w) = s.open().unwrap();
        // Nothing older is open: the watermark is the request itself.
        assert!(w <= e.seq && w > c.seq);
    }

    #[test]
    fn the_sequence_never_wraps() {
        let mut s = Seqs::new(1, 0);
        s.skip_to(OpId::MAX_SEQ);
        assert_eq!(s.open().unwrap().0.seq, OpId::MAX_SEQ);
        assert_eq!(s.open(), Err(Exhausted));
    }
}
