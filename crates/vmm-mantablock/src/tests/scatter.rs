// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Split requests over guest memory that does not line up with pages.

use std::io;
use std::sync::{Arc, Mutex};

use manta_block_ring::layout::PAGE;
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use super::{pattern, setup_for};
use crate::Job;

/// Guest memory as uneven runs scattered over a larger buffer, the way
/// a PRP1 offset and a PRP list lay it out.
struct Scatter {
    id: u32,
    tag: u16,
    op: BlockOp,
    off: u64,
    backing: Arc<Mutex<Vec<u8>>>,
    /// `(where in the backing, len)`, in transfer order.
    runs: Vec<(usize, usize)>,
}

impl Scatter {
    fn new(id: u32, tag: u16, op: BlockOp, lba: u64, blocks: usize) -> Self {
        let mut runs = Vec::new();
        let (mut left, mut at, mut i) = (blocks * PAGE, 64, 0usize);
        while left > 0 {
            // Dword multiples that never line up with a page.
            let n = [4, 4092, 4096, 1000, 3096, 12, 6000][i % 7].min(left);
            runs.push((at, n));
            at += n + 4 * (i + 1);
            left -= n;
            i += 1;
        }
        Self {
            id,
            tag,
            op,
            off: lba * PAGE as u64,
            backing: Arc::new(Mutex::new(vec![0; at])),
            runs,
        }
    }

    fn put(&self, data: &[u8]) {
        let mut b = self.backing.lock().unwrap();
        let mut at = 0;
        for &(pos, n) in &self.runs {
            b[pos..pos + n].copy_from_slice(&data[at..at + n]);
            at += n;
        }
    }

    fn get(&self) -> Vec<u8> {
        let b = self.backing.lock().unwrap();
        self.runs
            .iter()
            .flat_map(|&(pos, n)| b[pos..pos + n].iter().copied())
            .collect()
    }

    /// Calls `f` with each backing range of transfer bytes
    /// `at..at + len`, and where it falls in those bytes.
    fn each(
        &self,
        at: usize,
        len: usize,
        mut f: impl FnMut(std::ops::Range<usize>, std::ops::Range<usize>),
    ) {
        let mut start = 0;
        for &(pos, n) in &self.runs {
            let lo = at.max(start);
            let hi = (at + len).min(start + n);
            if lo < hi {
                f(pos + lo - start..pos + hi - start, lo - at..hi - at);
            }
            start += n;
        }
    }
}

impl Job for Scatter {
    type Id = u32;
    type Pages = ();

    fn id(&self) -> u32 {
        self.id
    }

    fn queue(&self) -> u16 {
        1
    }

    fn tag(&self) -> u16 {
        self.tag
    }

    fn op(&self) -> BlockOp {
        self.op
    }

    fn byte_offset(&self) -> u64 {
        self.off
    }

    fn len(&self) -> usize {
        self.runs.iter().map(|r| r.1).sum()
    }

    fn pages(&self) -> io::Result<()> {
        Ok(())
    }

    fn copy_from_guest(
        &self,
        _: &(),
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()> {
        let b = self.backing.lock().unwrap();
        self.each(at, dst.len(), |from, to| dst[to].copy_from_slice(&b[from]));
        Ok(())
    }

    fn copy_to_guest(&self, _: &(), at: usize, src: &[u8]) -> io::Result<()> {
        let mut b = self.backing.lock().unwrap();
        self.each(at, src.len(), |to, from| b[to].copy_from_slice(&src[from]));
        Ok(())
    }
}

#[test]
fn split_parts_copy_straight_between_misaligned_guest_runs_and_slots() {
    // Five pages per slot and four tags: 13 blocks go as 5, 5 and 3.
    let (ring, calls, mut eng) = setup_for::<Scatter>(4, 32, 5, true);
    let data = pattern(13, 0x5a);
    let w = Scatter::new(1, 0, BlockOp::Write { fua: false }, 40, 13);
    w.put(&data);
    ring.submit(w);
    let parts = eng.take_n(3);
    assert_eq!(
        parts.iter().map(|e| e.blocks).collect::<Vec<_>>(),
        vec![5, 5, 3]
    );
    let cqes: Vec<_> = parts.iter().map(|e| eng.serve(e).encode()).collect();
    eng.post(&cqes);
    calls.wait_for(1);
    assert_eq!(&eng.disk[40 * PAGE..53 * PAGE], &data[..]);

    let r = Arc::new(Scatter::new(2, 1, BlockOp::Read, 40, 13));
    ring.submit(Scatter {
        backing: Arc::clone(&r.backing),
        runs: r.runs.clone(),
        ..Scatter::new(2, 1, BlockOp::Read, 40, 13)
    });
    eng.serve_all(3);
    assert_eq!(calls.wait_for(2)[1], (2, BlockStatus::Success));
    assert_eq!(r.get(), data);
    // Nothing lands between the runs.
    let b = r.backing.lock().unwrap();
    let mut next = 0;
    for &(pos, n) in &r.runs {
        assert!(b[next..pos].iter().all(|&x| x == 0));
        next = pos + n;
    }
    assert!(b[next..].iter().all(|&x| x == 0));
}
