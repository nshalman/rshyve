// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest requests in the ring's 4 KiB blocks.
//!
//! A 512e guest names 512-byte sectors. The ring moves whole 4 KiB
//! blocks, and a write that covers a block only in part goes with its
//! byte range, which the engine merges. A read always moves whole blocks;
//! only the guest's bytes are copied out.

use manta_block_ring::layout::PAGE;

const BLOCK: u64 = PAGE as u64;

/// The 4 KiB blocks a data request touches, and its guest bytes in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) lba: u64,
    pub(crate) blocks: u32,
    /// Where the guest range starts in the first block.
    pub(crate) head: u32,
    /// Guest bytes; 0 when they are every byte of the blocks.
    pub(crate) len: u64,
}

/// The span of `len` bytes at byte `off`, on a volume of `sector`-byte
/// sectors. `None` when the request does not name whole sectors, or the
/// blocks do not fit a u32.
pub(crate) fn data_span(off: u64, len: u64, sector: u32) -> Option<Span> {
    let s = u64::from(sector);
    if len == 0 || s == 0 || !off.is_multiple_of(s) || !len.is_multiple_of(s) {
        return None;
    }
    let end = off.checked_add(len)?;
    let lba = off / BLOCK;
    let blocks = u32::try_from(end.div_ceil(BLOCK) - lba).ok()?;
    let head = (off % BLOCK) as u32;
    let whole = head == 0 && end.is_multiple_of(BLOCK);
    Some(Span {
        lba,
        blocks,
        head,
        len: if whole { 0 } else { len },
    })
}

/// The bytes of one part of a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PartBytes {
    /// As the SQE carries them: 0 and 0 when the part is whole blocks.
    pub(crate) off: u16,
    pub(crate) len: u32,
    /// Where the guest bytes start in the part's blocks.
    pub(crate) in_part: usize,
    /// Where they start in the transfer.
    pub(crate) at: usize,
    /// Guest bytes in the part.
    pub(crate) glen: usize,
}

/// The guest bytes of blocks `first..first + n` of a span with a byte
/// range. The parts of one span never overlap and cover its range once.
pub(crate) fn part_bytes(sp: &Span, first: u32, n: u32) -> PartBytes {
    let from = u64::from(first) * BLOCK;
    let to = from + u64::from(n) * BLOCK;
    let g = (u64::from(sp.head), u64::from(sp.head) + sp.len);
    let (s, e) = (g.0.max(from), g.1.min(to));
    let whole = s == from && e == to;
    PartBytes {
        off: if whole { 0 } else { (s - from) as u16 },
        len: if whole { 0 } else { (e - s) as u32 },
        in_part: (s - from) as usize,
        at: (s - g.0) as usize,
        glen: e.saturating_sub(s) as usize,
    }
}

/// One deallocate or write zeroes on the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZeroPart {
    pub(crate) lba: u64,
    pub(crate) blocks: u32,
    /// A byte range in a single block; 0 and 0 for whole blocks.
    pub(crate) off: u16,
    pub(crate) len: u32,
}

/// The ring ops of zeroing `len` bytes at `off`: each sub-block edge
/// alone with its byte range, the whole blocks between as few ops as a
/// u32 block count allows.
pub(crate) fn zero_parts(off: u64, len: u64, out: &mut Vec<ZeroPart>) {
    let Some(end) = off.checked_add(len) else {
        return;
    };
    let mut at = off;
    while at < end {
        let lba = at / BLOCK;
        let in_block = at % BLOCK;
        if in_block != 0 || end - at < BLOCK {
            let stop = end.min((lba + 1) * BLOCK);
            out.push(ZeroPart {
                lba,
                blocks: 1,
                off: in_block as u16,
                len: (stop - at) as u32,
            });
            at = stop;
            continue;
        }
        let blocks = ((end - at) / BLOCK).min(u64::from(u32::MAX));
        out.push(ZeroPart {
            lba,
            blocks: blocks as u32,
            off: 0,
            len: 0,
        });
        at += blocks * BLOCK;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_spans_carry_no_range() {
        assert_eq!(
            data_span(8192, 8192, 512),
            Some(Span {
                lba: 2,
                blocks: 2,
                head: 0,
                len: 0
            })
        );
        assert_eq!(data_span(4096, 4096, 4096).map(|s| s.len), Some(0));
        assert_eq!(data_span(512, 4096, 4096), None);
        assert_eq!(data_span(100, 512, 512), None);
        assert_eq!(data_span(0, 0, 512), None);
    }

    #[test]
    fn a_range_splits_into_parts_that_cover_it_once() {
        // Sectors 7 to 25: the end of block 0, blocks 1 and 2, the start
        // of block 3, in parts of two blocks.
        let sp = data_span(7 * 512, 19 * 512, 512).unwrap();
        assert_eq!((sp.lba, sp.blocks, sp.head), (0, 4, 3584));
        let a = part_bytes(&sp, 0, 2);
        assert_eq!(
            (a.off, a.len, a.in_part, a.at, a.glen),
            (3584, 4608, 3584, 0, 4608)
        );
        let b = part_bytes(&sp, 2, 2);
        assert_eq!((b.off, b.len, b.in_part, b.at), (0, 5120, 0, 4608));
        assert_eq!(a.glen + b.glen, 19 * 512);
        // A middle part that the range covers whole carries no range.
        let sp = data_span(512, 3 * 4096, 512).unwrap();
        let m = part_bytes(&sp, 1, 2);
        assert_eq!((m.off, m.len, m.glen), (0, 0, 8192));
    }

    #[test]
    fn zeroing_splits_edges_from_whole_blocks() {
        let mut v = Vec::new();
        zero_parts(512, 3 * 4096, &mut v);
        assert_eq!(
            v,
            vec![
                ZeroPart {
                    lba: 0,
                    blocks: 1,
                    off: 512,
                    len: 3584
                },
                ZeroPart {
                    lba: 1,
                    blocks: 2,
                    off: 0,
                    len: 0
                },
                ZeroPart {
                    lba: 3,
                    blocks: 1,
                    off: 0,
                    len: 512
                },
            ]
        );
        v.clear();
        zero_parts(1024, 1024, &mut v);
        assert_eq!(
            v,
            vec![ZeroPart {
                lba: 0,
                blocks: 1,
                off: 1024,
                len: 1024
            }]
        );
        v.clear();
        zero_parts(8192, 4096, &mut v);
        assert_eq!(
            v,
            vec![ZeroPart {
                lba: 2,
                blocks: 1,
                off: 0,
                len: 0
            }]
        );
        v.clear();
        // More blocks than a u32 holds go as more than one op.
        zero_parts(0, (u64::from(u32::MAX) + 3) * 4096, &mut v);
        assert_eq!(v.len(), 2);
        assert_eq!(
            (v[0].blocks, v[1].blocks, v[1].lba),
            (u32::MAX, 3, u64::from(u32::MAX))
        );
    }
}
