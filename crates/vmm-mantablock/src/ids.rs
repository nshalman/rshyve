// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Request ids of one queue.
//!
//! `op_id = generation << 64 | queue << 48 | seq`. The sequence must
//! never repeat in a generation: the engine and the log take a repeated
//! op id as a replay.

use manta_block_ring::OpId;

#[derive(Debug)]
pub(crate) struct Seqs {
    generation: u64,
    queue: u16,
    next: u64,
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
        }
    }

    pub(crate) fn open(&mut self) -> Result<OpId, Exhausted> {
        let seq = self.next;
        if seq > OpId::MAX_SEQ {
            return Err(Exhausted);
        }
        self.next += 1;
        Ok(OpId {
            generation: self.generation,
            queue: self.queue,
            seq,
        })
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
        let id = s.open().unwrap();
        let id2 = s.open().unwrap();
        assert_eq!(id.to_u128(), (0xabcd_u128 << 64) | (3 << 48));
        assert_eq!(id2.to_u128(), (0xabcd_u128 << 64) | (3 << 48) | 1);
        assert_eq!(OpId::from_u128(id2.to_u128()), id2);
    }

    #[test]
    fn the_sequence_never_wraps() {
        let mut s = Seqs::new(1, 0);
        s.skip_to(OpId::MAX_SEQ);
        assert_eq!(s.open().unwrap().seq, OpId::MAX_SEQ);
        assert_eq!(s.open(), Err(Exhausted));
    }
}
