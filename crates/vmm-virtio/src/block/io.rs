// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host-buffered I/O between a virtio-blk chain and the disk.
//!
//! A backing-store call owns only host memory: a request is bounced
//! through the worker's buffer. A read fills that buffer with no
//! guest-access permission held and copies it out under permission; a
//! write takes its snapshot under permission and then reaches the disk
//! from the snapshot. Nothing here holds permission across a syscall,
//! which is what keeps a device reset bounded.

use std::fs::File;
use std::os::unix::fs::FileExt;

use vmm_core::mem::PhysMap;

use super::{bits, ChainBuf, VirtioBlkReqHdr, BLK_REQ_HEADER_SIZE};
use super::{SEG_MAX, SIZE_MAX};

/// Bytes a worker moves between the disk and its buffer per call.
///
/// One guest-memory copy of this size is the longest step a reset
/// waits for, so this constant is the drain bound. A larger request is
/// split into this many bytes at a time.
pub(super) const BOUNCE_BYTES: usize = 256 * 1024;

/// Data segments one request may carry. Matches the advertised
/// `seg_max`.
pub(super) const MAX_SEGMENTS: usize = SEG_MAX as usize;

/// Bytes one request may move. Matches the advertised `seg_max` and
/// `size_max`, which is the most a driver that reads them can build.
pub(super) const MAX_REQ_BYTES: usize = MAX_SEGMENTS * SIZE_MAX as usize;

/// Bytes one request may move when the driver did not agree to both
/// limits. edk2's VirtioBlkDxe negotiates neither and puts a read of up
/// to 1 GiB in one descriptor.
pub(super) const MAX_UNLIMITED_REQ_BYTES: usize = 1 << 30;

/// The bounds a request is held to.
///
/// VirtIO 1.3 sec 5.2.3: `seg_max` and `size_max` bind the driver only
/// when it negotiated the feature bit. A driver that did not may send
/// a larger segment, and refusing it fails a correct request. The
/// firmware loads its boot file that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReqLimits {
    segments: usize,
    seg_bytes: u32,
    total: usize,
}

impl ReqLimits {
    /// The bounds that apply when no segment limit was negotiated.
    pub(super) const UNLIMITED: Self = Self {
        segments: usize::MAX,
        seg_bytes: u32::MAX,
        total: MAX_UNLIMITED_REQ_BYTES,
    };

    /// The bounds for the negotiated feature bits.
    pub(super) fn negotiated(features: u64) -> Self {
        let seg_max = features & bits::VIRTIO_BLK_F_SEG_MAX != 0;
        let size_max = features & bits::VIRTIO_BLK_F_SIZE_MAX != 0;
        Self {
            segments: if seg_max { MAX_SEGMENTS } else { usize::MAX },
            seg_bytes: if size_max { SIZE_MAX } else { u32::MAX },
            total: if seg_max && size_max {
                MAX_REQ_BYTES
            } else {
                MAX_UNLIMITED_REQ_BYTES
            },
        }
    }
}

/// Fold the data segments of a chain, refusing one that runs past
/// `limits`.
///
/// Publishing `size_max` and `seg_max` bounds nothing on its own. The
/// guest builds the chain and picks both numbers, so they hold only
/// where they are checked.
fn fold(
    chain: &[ChainBuf],
    limits: &ReqLimits,
    pick: impl Fn(&ChainBuf) -> bool,
) -> Option<usize> {
    let end = chain.len().checked_sub(1)?;
    let data = chain.get(1..end)?;
    if data.len() > limits.segments {
        return None;
    }
    let mut total = 0usize;
    let mut wanted = 0usize;
    for buf in data {
        let len = match buf {
            ChainBuf::Writable { len, .. } | ChainBuf::Readable { len, .. } => {
                *len
            }
        };
        if len > limits.seg_bytes {
            return None;
        }
        total = total.checked_add(len as usize)?;
        if total > limits.total {
            return None;
        }
        if pick(buf) {
            wanted = wanted.checked_add(len as usize)?;
        }
    }
    Some(wanted)
}

/// Bytes a chain names in either direction, which is what the
/// sector-range check must cover.
pub(super) fn span(chain: &[ChainBuf], limits: &ReqLimits) -> Option<usize> {
    fold(chain, limits, |_| true)
}

/// Bytes a chain moves in the request's own direction.
///
/// The chain passed [`span`] under the negotiated limits before it was
/// dispatched, so only the loosest bound is checked again here.
pub(super) fn transfer(chain: &[ChainBuf], writable: bool) -> Option<usize> {
    fold(chain, &ReqLimits::UNLIMITED, |buf| {
        matches!(buf, ChainBuf::Writable { .. }) == writable
    })
}

/// Byte offset of `sector` in the backing store.
pub(super) fn sector_offset(sector: u64) -> Option<u64> {
    let offset = sector.checked_mul(bits::SECTOR_SIZE)?;
    // The kernel takes a signed offset. A larger one would wrap to a
    // place the caller did not name.
    i64::try_from(offset).ok()?;
    Some(offset)
}

pub(super) fn parse_header(
    chain: &[ChainBuf],
    physmap: &PhysMap,
) -> Option<VirtioBlkReqHdr> {
    match &chain[0] {
        ChainBuf::Readable { addr, len }
            if (*len as usize) >= BLK_REQ_HEADER_SIZE =>
        {
            physmap
                .lookup(*addr, BLK_REQ_HEADER_SIZE)
                .and_then(|sub| sub.read::<VirtioBlkReqHdr>().ok())
        }
        _ => None,
    }
}

/// Run `visit` over the part of each matching data segment that falls
/// in the window `[offset, offset + len)` of the request.
///
/// Returns false when a segment cannot be mapped or a copy fails. The
/// caller must then abandon the transfer: a partly filled buffer holds
/// the previous request's bytes.
fn walk_window(
    chain: &[ChainBuf],
    writable: bool,
    offset: usize,
    len: usize,
    mut visit: impl FnMut(u64, usize, usize) -> bool,
) -> bool {
    let Some(end) = offset.checked_add(len) else {
        return false;
    };
    let Some(data) = chain.len().checked_sub(1).and_then(|e| chain.get(1..e))
    else {
        return false;
    };
    let mut pos = 0usize;
    let mut covered = 0usize;
    for buf in data {
        let (addr, seg) = match buf {
            ChainBuf::Writable { addr, len } if writable => {
                (*addr, *len as usize)
            }
            ChainBuf::Readable { addr, len } if !writable => {
                (*addr, *len as usize)
            }
            _ => continue,
        };
        let start = pos;
        let Some(stop) = start.checked_add(seg) else {
            return false;
        };
        pos = stop;
        if stop <= offset || start >= end {
            continue;
        }
        let from = offset.max(start);
        let to = end.min(stop);
        if to == from {
            continue;
        }
        let Some(gpa) = addr.checked_add((from - start) as u64) else {
            return false;
        };
        if !visit(gpa, from - offset, to - from) {
            return false;
        }
        covered += to - from;
    }
    // A short walk would leave the tail of the window untouched, which
    // for a read means the guest keeps whatever the page held before.
    covered == len
}

/// Copy `buf` into the chain's writable data segments, `offset` bytes
/// into the request.
pub(super) fn scatter(
    physmap: &PhysMap,
    chain: &[ChainBuf],
    offset: usize,
    buf: &[u8],
) -> bool {
    walk_window(chain, true, offset, buf.len(), |gpa, at, len| {
        let Some(sub) = physmap.lookup(gpa, len) else {
            return false;
        };
        // A bulk copy, not the volatile byte loop: the driver may not
        // read a data buffer before the used index that publishes it,
        // and that index is written after this with a release fence.
        // So no vCPU can observe this copy part way through.
        sub.copy_in(&buf[at..at + len]).is_ok()
    })
}

/// Copy the chain's readable data segments into `buf`, `offset` bytes
/// into the request.
pub(super) fn gather(
    physmap: &PhysMap,
    chain: &[ChainBuf],
    offset: usize,
    buf: &mut [u8],
) -> bool {
    let want = buf.len();
    walk_window(chain, false, offset, want, |gpa, at, len| {
        let Some(sub) = physmap.lookup(gpa, len) else {
            return false;
        };
        // The driver owns the buffer until the used entry lands, so it
        // may not rewrite it here. A bulk copy matches what the kernel
        // did when this was a `pwritev` straight out of guest memory.
        sub.copy_out(&mut buf[at..at + len]).is_ok()
    })
}

/// Fill the whole of `buf` from the disk. Host memory only, so no
/// guest-access permission is needed or held.
///
/// End of file short of the range is a failure. The caller must not
/// copy the buffer out then: past the bytes read it still holds the
/// last request.
pub(super) fn read_backend(file: &File, offset: u64, buf: &mut [u8]) -> bool {
    file.read_exact_at(buf, offset).is_ok()
}

/// Write the whole of `buf` to the disk from the host snapshot.
pub(super) fn write_backend(file: &File, offset: u64, buf: &[u8]) -> bool {
    file.write_all_at(buf, offset).is_ok()
}

pub(super) fn write_status(
    physmap: &PhysMap,
    chain: &[ChainBuf],
    status: u8,
) -> u32 {
    if let Some(ChainBuf::Writable { addr, len }) = chain.last() {
        if *len >= 1 {
            if let Some(sub) = physmap.lookup(*addr, 1) {
                // write to guest memory; no recovery if unmapped
                let _ = sub.write_bytes(&[status]);
            }
        }
    }
    1
}
