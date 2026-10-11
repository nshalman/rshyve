// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PRP walking, NVMe 1.4 section 4.3.
//!
//! PRP1 may start anywhere in a page and covers to the end of it. If
//! the rest fits in one page, PRP2 names that page and must be page
//! aligned. Otherwise PRP2 names a PRP list: it may start anywhere in
//! its page, ends at the page boundary, and its last entry chains to
//! the next list while data remains.

use std::io;

use vmm_core::common::PAGE_SIZE;
use vmm_core::mem::{GuestIoVec, MemCtx, SubMapping};

const PAGE_OFFSET: u64 = PAGE_SIZE as u64 - 1;
const ENTRIES_PER_LIST: u64 = PAGE_SIZE as u64 / 8;

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

enum Next {
    Prp1,
    Prp2,
    List { base: u64, idx: u64 },
    Done,
}

/// The segments a command's PRPs name, produced one at a time.
struct PrpWalk<'a> {
    mem: &'a MemCtx,
    prp1: u64,
    prp2: u64,
    remain: usize,
    next: Next,
}

impl<'a> PrpWalk<'a> {
    fn new(mem: &'a MemCtx, prp1: u64, prp2: u64, len: usize) -> Self {
        Self {
            mem,
            prp1,
            prp2,
            remain: len,
            next: if len == 0 { Next::Done } else { Next::Prp1 },
        }
    }

    fn list_entry(&self, base: u64, idx: u64) -> io::Result<u64> {
        let gpa = base
            .checked_add(idx.checked_mul(8).ok_or_else(|| invalid("PRP list"))?)
            .ok_or_else(|| invalid("PRP list address wraps"))?;
        let mut buf = [0u8; 8];
        self.mem.read(gpa, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    /// The next `(gpa, len)` segment, or `None` when the transfer is
    /// covered.
    fn next_segment(&mut self) -> io::Result<Option<(u64, usize)>> {
        let (gpa, len, next) = match self.next {
            Next::Done => return Ok(None),
            Next::Prp1 => {
                if self.prp1 & 3 != 0 {
                    return Err(invalid("PRP1 is not dword aligned"));
                }
                let offset = (self.prp1 & PAGE_OFFSET) as usize;
                let len = (PAGE_SIZE - offset).min(self.remain);
                let after = self.remain - len;
                let next = if after == 0 {
                    Next::Done
                } else if after <= PAGE_SIZE {
                    Next::Prp2
                } else {
                    if self.prp2 & 7 != 0 {
                        return Err(invalid("PRP list is not qword aligned"));
                    }
                    Next::List {
                        base: self.prp2 & !PAGE_OFFSET,
                        idx: (self.prp2 & PAGE_OFFSET) / 8,
                    }
                };
                (self.prp1, len, next)
            }
            Next::Prp2 => {
                if self.prp2 & PAGE_OFFSET != 0 {
                    return Err(invalid("PRP2 has a page offset"));
                }
                (self.prp2, self.remain, Next::Done)
            }
            Next::List { base, idx } => {
                let entry = self.list_entry(base, idx)?;
                if entry & PAGE_OFFSET != 0 {
                    return Err(invalid("PRP list entry has a page offset"));
                }
                if self.remain <= PAGE_SIZE {
                    (entry, self.remain, Next::Done)
                } else if idx + 1 < ENTRIES_PER_LIST {
                    (entry, PAGE_SIZE, Next::List { base, idx: idx + 1 })
                } else {
                    // The last entry of a list points at the next list.
                    self.next = Next::List {
                        base: entry,
                        idx: 0,
                    };
                    return self.next_segment();
                }
            }
        };
        self.remain -= len;
        self.next = next;
        Ok(Some((gpa, len)))
    }
}

/// Pin the pages a transfer of `total_len` bytes names.
pub(super) fn prp_to_iovec(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    total_len: usize,
) -> io::Result<GuestIoVec> {
    let mut iov = GuestIoVec::with_capacity(16);
    let mut walk = PrpWalk::new(mem, prp1, prp2, total_len);
    while let Some((gpa, len)) = walk.next_segment()? {
        let sub = mem
            .lookup(gpa, len)
            .ok_or_else(|| invalid("PRP names unmapped memory"))?;
        iov.push(sub);
    }
    Ok(iov)
}

/// The guest runs a transfer's PRPs name, read once, so a backend that
/// moves the transfer in pieces does not walk the PRP list per piece.
#[derive(Debug, Clone, Default)]
pub struct Segments {
    /// `(start in the transfer, gpa, len)`.
    runs: Vec<(usize, u64, usize)>,
    len: usize,
}

impl Segments {
    /// Bytes the runs cover.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Calls `f` with each mapped run of `len` bytes from `at`, and where
    /// the run falls in those `len` bytes.
    fn for_range(
        &self,
        mem: &MemCtx,
        at: usize,
        len: usize,
        mut f: impl FnMut(&SubMapping, std::ops::Range<usize>) -> io::Result<()>,
    ) -> io::Result<()> {
        let end = at
            .checked_add(len)
            .filter(|&end| end <= self.len)
            .ok_or_else(|| invalid("range is past the transfer"))?;
        let mut i = self
            .runs
            .partition_point(|&(start, _, _)| start <= at)
            .saturating_sub(1);
        let mut pos = at;
        while pos < end {
            let &(start, gpa, seg) =
                self.runs.get(i).ok_or_else(|| invalid("short PRP walk"))?;
            let off = pos - start;
            let n = (seg - off).min(end - pos);
            let gpa = gpa
                .checked_add(off as u64)
                .ok_or_else(|| invalid("PRP address wraps"))?;
            let sub = mem
                .lookup(gpa, n)
                .ok_or_else(|| invalid("PRP names unmapped memory"))?;
            f(&sub, pos - at..pos - at + n)?;
            pos += n;
            i += 1;
        }
        Ok(())
    }

    /// The plain copy of [`copy_from_prp`], of part of the transfer.
    pub(super) fn copy_out_at(
        &self,
        mem: &MemCtx,
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()> {
        self.for_range(mem, at, dst.len(), |sub, range| {
            sub.copy_out(&mut dst[range])
        })
    }

    pub(super) fn copy_in_at(
        &self,
        mem: &MemCtx,
        at: usize,
        src: &[u8],
    ) -> io::Result<()> {
        self.for_range(mem, at, src.len(), |sub, range| {
            sub.copy_in(&src[range])
        })
    }
}

pub(super) fn prp_segments(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    len: usize,
) -> io::Result<Segments> {
    let mut segs = Segments::default();
    let mut walk = PrpWalk::new(mem, prp1, prp2, len);
    while let Some((gpa, seg)) = walk.next_segment()? {
        segs.runs.push((segs.len, gpa, seg));
        segs.len += seg;
    }
    Ok(segs)
}

/// Write `data` to the pages the PRPs name.
pub(super) fn write_to_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    data: &[u8],
) -> io::Result<()> {
    let mut walk = PrpWalk::new(mem, prp1, prp2, data.len());
    let mut offset = 0;
    while let Some((gpa, len)) = walk.next_segment()? {
        mem.write(gpa, &data[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}

/// Walk the PRPs of a `len`-byte transfer, one mapped run at a time.
fn for_each_run(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    len: usize,
    mut f: impl FnMut(&SubMapping, std::ops::Range<usize>) -> io::Result<()>,
) -> io::Result<()> {
    let mut walk = PrpWalk::new(mem, prp1, prp2, len);
    let mut offset = 0usize;
    while let Some((gpa, seg)) = walk.next_segment()? {
        let sub = mem
            .lookup(gpa, seg)
            .ok_or_else(|| invalid("PRP names unmapped memory"))?;
        let end = offset
            .checked_add(seg)
            .ok_or_else(|| invalid("PRP walk overran the transfer"))?;
        f(&sub, offset..end)?;
        offset = end;
    }
    Ok(())
}

/// A plain copy: the driver gave these pages to the device, so a torn
/// read only tears data the guest itself is still changing.
pub(super) fn copy_from_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    len: usize,
    dst: &mut [u8],
) -> io::Result<()> {
    if dst.len() != len {
        return Err(invalid("buffer is not the transfer length"));
    }
    for_each_run(mem, prp1, prp2, len, |sub, range| {
        let part = dst
            .get_mut(range)
            .ok_or_else(|| invalid("PRP walk overran the transfer"))?;
        sub.copy_out(part)
    })
}

pub(super) fn copy_to_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    len: usize,
    src: &[u8],
) -> io::Result<()> {
    if src.len() != len {
        return Err(invalid("buffer is not the transfer length"));
    }
    for_each_run(mem, prp1, prp2, len, |sub, range| {
        let part = src
            .get(range)
            .ok_or_else(|| invalid("PRP walk overran the transfer"))?;
        sub.copy_in(part)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vmm_core::mem::PhysMap;

    use super::*;

    #[test]
    fn segments_copy_any_range_of_a_prp_list_transfer() {
        let mem = MemCtx::new(Arc::new(PhysMap::new_anon(0, 0x10000).unwrap()));
        // PRP1 four bytes short of a page end, then a list of three
        // pages out of order, the last one partly used.
        let list = [0x2000u64, 0x5000, 0x3000];
        let bytes: Vec<u8> =
            list.iter().flat_map(|e| e.to_le_bytes()).collect();
        mem.write(0x8000, &bytes).unwrap();
        let len = 4 + 2 * PAGE_SIZE + 1000;
        let segs = prp_segments(&mem, 0x0ffc, 0x8000, len).unwrap();
        assert_eq!(
            segs.runs,
            vec![
                (0, 0x0ffc, 4),
                (4, 0x2000, PAGE_SIZE),
                (4 + PAGE_SIZE, 0x5000, PAGE_SIZE),
                (4 + 2 * PAGE_SIZE, 0x3000, 1000),
            ]
        );
        assert_eq!(segs.len(), len);

        let mut model = vec![0u8; len];
        for (n, (at, piece)) in [
            (0, len),
            (2, 7),
            (3, PAGE_SIZE + 2),
            (4 + PAGE_SIZE - 1, 1002),
        ]
        .into_iter()
        .enumerate()
        {
            let src: Vec<u8> = (0..piece).map(|i| (i * 7 + n) as u8).collect();
            segs.copy_in_at(&mem, at, &src).unwrap();
            model[at..at + piece].copy_from_slice(&src);
        }
        let mut whole = vec![0u8; len];
        copy_from_prp(&mem, 0x0ffc, 0x8000, len, &mut whole).unwrap();
        assert_eq!(whole, model);
        let mut piece = vec![0u8; PAGE_SIZE + 10];
        segs.copy_out_at(&mem, 1, &mut piece).unwrap();
        assert_eq!(piece, model[1..PAGE_SIZE + 11]);

        assert!(segs.copy_out_at(&mem, len - 1, &mut [0; 2]).is_err());
        assert!(segs.copy_in_at(&mem, usize::MAX, &[0]).is_err());
        assert!(segs.copy_in_at(&mem, len, &[]).is_ok());
    }
}
