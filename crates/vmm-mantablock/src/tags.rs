// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring tags of one queue: which are free, and what each one in
//! flight carries. This is all the submission side and the completion
//! thread share without a lock.
//!
//! Only the submission side takes a free tag and fills its entry, before
//! the part goes on the ring. Only the completion thread checks and
//! empties an entry, then frees the tag. So an entry is never written by
//! both at once, and the free bitmap orders the hand-over both ways.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use manta_block_ring::{BadCompletion, Cqe};

/// An entry with no part. No op id has it: the queue fits in 4 bits.
const EMPTY: u64 = u64::MAX;

/// One part on the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Part {
    /// The guest slot of its request.
    pub(crate) req: usize,
    pub(crate) first: u32,
    /// 0 for a flush.
    pub(crate) blocks: u32,
}

// The submission side fills one entry while the completion thread reads
// the next, so each gets its own cache line.
#[repr(align(64))]
struct Entry {
    /// `EMPTY`, or the low half of the op id of the part in flight.
    id: AtomicU64,
    /// `req << 32 | first`.
    at: AtomicU64,
    blocks: AtomicU32,
    /// Monotonic ns, for the stall check.
    posted: AtomicU64,
}

pub(crate) struct Tags {
    /// A set bit is a free tag.
    free: Vec<AtomicU64>,
    entries: Vec<Entry>,
}

impl Tags {
    pub(crate) fn new(count: u16) -> Self {
        let n = usize::from(count);
        let free = (0..n.div_ceil(64))
            .map(|w| {
                let bits = (n - w * 64).min(64);
                AtomicU64::new(if bits == 64 { !0 } else { (1 << bits) - 1 })
            })
            .collect();
        let entries = (0..n)
            .map(|_| Entry {
                id: AtomicU64::new(EMPTY),
                at: AtomicU64::new(0),
                blocks: AtomicU32::new(0),
                posted: AtomicU64::new(0),
            })
            .collect();
        Self { free, entries }
    }

    /// Words in the free bitmap.
    pub(crate) fn words(&self) -> usize {
        self.free.len()
    }

    /// Submission side only. The lowest free tag, so the same buffer
    /// slots stay warm.
    pub(crate) fn claim(&self) -> Option<u16> {
        for (w, word) in self.free.iter().enumerate() {
            // Pairs with `release`: the completion thread is done with
            // the entry and the slot before the tag shows free.
            let bits = word.load(Ordering::Acquire);
            if bits != 0 {
                let b = bits.trailing_zeros();
                word.fetch_and(!(1 << b), Ordering::Relaxed);
                return Some((w * 64) as u16 + b as u16);
            }
        }
        None
    }

    pub(crate) fn any_free(&self) -> bool {
        self.free.iter().any(|w| w.load(Ordering::Acquire) != 0)
    }

    /// Submission side, for a tag it claimed and did not post.
    pub(crate) fn unclaim(&self, tag: u16) {
        let t = usize::from(tag);
        self.free[t / 64].fetch_or(1 << (t % 64), Ordering::Release);
    }

    /// Submission side, before the part goes on the ring.
    pub(crate) fn open(&self, tag: u16, id: u64, p: Part, posted: u64) {
        let e = &self.entries[usize::from(tag)];
        e.at.store(
            ((p.req as u64) << 32) | u64::from(p.first),
            Ordering::Relaxed,
        );
        e.blocks.store(p.blocks, Ordering::Relaxed);
        e.posted.store(posted, Ordering::Relaxed);
        e.id.store(id, Ordering::Release);
    }

    /// Completion thread only. Checks `c` against the part in flight
    /// under its tag and empties the entry; the tag stays taken until
    /// [`Tags::release`].
    pub(crate) fn complete(&self, c: &Cqe) -> Result<Part, BadCompletion> {
        let e = self
            .entries
            .get(usize::from(c.tag))
            .ok_or(BadCompletion::Tag(c.tag))?;
        match e.id.load(Ordering::Acquire) {
            EMPTY => return Err(BadCompletion::Tag(c.tag)),
            id if id != c.op_seq => return Err(BadCompletion::OpSeq),
            _ => {}
        }
        e.id.store(EMPTY, Ordering::Relaxed);
        let at = e.at.load(Ordering::Relaxed);
        Ok(Part {
            req: (at >> 32) as usize,
            first: at as u32,
            blocks: e.blocks.load(Ordering::Relaxed),
        })
    }

    /// Completion thread only: frees the tags set in `freed`, one bit
    /// per tag, and clears it.
    pub(crate) fn release(&self, freed: &mut [u64]) {
        for (word, bits) in self.free.iter().zip(freed.iter_mut()) {
            if *bits != 0 {
                word.fetch_or(*bits, Ordering::Release);
                *bits = 0;
            }
        }
    }

    /// The lowest of `opened`, by tag, over the tags not free. Every
    /// part in flight holds a tag that is not free, so nothing open is
    /// older; a tag on its way back only makes the answer lower.
    pub(crate) fn oldest_open(&self, opened: &[u64]) -> Option<u64> {
        let mut low: Option<u64> = None;
        for (w, word) in self.free.iter().enumerate() {
            let n = (self.entries.len() - w * 64).min(64);
            let valid = if n == 64 { !0 } else { (1u64 << n) - 1 };
            let mut used = !word.load(Ordering::Relaxed) & valid;
            while used != 0 {
                let t = w * 64 + used.trailing_zeros() as usize;
                used &= used - 1;
                let s = opened[t];
                low = Some(low.map_or(s, |l| l.min(s)));
            }
        }
        low
    }

    /// When the oldest part in flight was posted.
    pub(crate) fn oldest_posted(&self) -> Option<u64> {
        self.entries
            .iter()
            .filter(|e| e.id.load(Ordering::Acquire) != EMPTY)
            .map(|e| e.posted.load(Ordering::Relaxed))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use manta_block_ring::Status;

    use super::*;

    fn cqe(tag: u16, op_seq: u64) -> Cqe {
        Cqe {
            tag,
            status: Status::Success,
            op_seq,
            engine_ns: 0,
            durable_ns: 0,
        }
    }

    fn part(req: usize) -> Part {
        Part {
            req,
            first: 7,
            blocks: 3,
        }
    }

    #[test]
    fn tags_go_lowest_first_and_come_back_only_when_released() {
        let t = Tags::new(70);
        assert_eq!(t.words(), 2);
        let got: Vec<_> = (0..70).map(|_| t.claim().unwrap()).collect();
        assert_eq!(got, (0..70).collect::<Vec<u16>>());
        assert_eq!(t.claim(), None);
        assert!(!t.any_free());

        t.open(65, 1000, part(4), 9);
        assert_eq!(t.complete(&cqe(65, 1000)), Ok(part(4)));
        // Emptied but not released: still taken.
        assert_eq!(t.claim(), None);
        let mut freed = vec![0, 1 << 1];
        t.release(&mut freed);
        assert_eq!(freed, vec![0, 0]);
        assert_eq!(t.claim(), Some(65));
        t.unclaim(3);
        assert_eq!(t.claim(), Some(3));
    }

    #[test]
    fn a_completion_must_match_the_part_in_flight() {
        let t = Tags::new(8);
        let tag = t.claim().unwrap();
        assert_eq!(t.complete(&cqe(tag, 5)), Err(BadCompletion::Tag(tag)));
        t.open(tag, 5, part(1), 0);
        assert_eq!(t.complete(&cqe(tag, 6)), Err(BadCompletion::OpSeq));
        assert_eq!(t.complete(&cqe(9, 5)), Err(BadCompletion::Tag(9)));
        assert!(t.complete(&cqe(tag, 5)).is_ok());
        // A second completion for the same part.
        assert_eq!(t.complete(&cqe(tag, 5)), Err(BadCompletion::Tag(tag)));
    }

    #[test]
    fn the_watermark_is_the_oldest_tag_not_free() {
        let t = Tags::new(4);
        let mut opened = vec![0; 4];
        assert_eq!(t.oldest_open(&opened), None);
        for seq in 10..13 {
            let tag = t.claim().unwrap();
            opened[usize::from(tag)] = seq;
            t.open(tag, seq, part(0), seq);
        }
        assert_eq!(t.oldest_open(&opened), Some(10));
        assert_eq!(t.oldest_posted(), Some(10));
        // Out of order, as the engine may complete.
        t.complete(&cqe(1, 11)).unwrap();
        t.release(&mut [1 << 1]);
        assert_eq!(t.oldest_open(&opened), Some(10));
        t.complete(&cqe(0, 10)).unwrap();
        // Emptied, not yet released: counted, which only lowers it.
        assert_eq!(t.oldest_open(&opened), Some(10));
        assert_eq!(t.oldest_posted(), Some(12));
        t.release(&mut [1]);
        assert_eq!(t.oldest_open(&opened), Some(12));
    }
}
