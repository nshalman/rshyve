// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DISCARD and WRITE_ZEROES payload handling (VirtIO 1.3, 5.2.6.2).
//!
//! The two commands share a payload but not their guarantees. After
//! WRITE_ZEROES the range MUST read back as zeros, so it writes. After
//! DISCARD the data is unspecified, so a validated request may complete
//! without touching the backing store.
//!
//! Answering OK without doing either is not a missing hint. ext4 routes
//! inode-table initialisation and unwritten-extent conversion through
//! `sb_issue_zeroout`, which has no fallback when the device reports
//! success, so the guest keeps whatever the backing store held before.

use vmm_core::mem::PhysMap;

use super::DiskLimits;
use crate::bits;
use crate::queue::ChainBuf;

/// Size of one `struct virtio_blk_discard_write_zeroes`.
const SEG_SIZE: usize = 16;

/// Segments accepted in one request of either kind. Published as
/// `max_discard_seg` and `max_write_zeroes_seg`.
pub(super) const MAX_SEG: u32 = 8;

/// Sectors per DISCARD segment (2 GiB). DISCARD does no I/O, so this
/// only bounds how much a guest can name in one segment.
pub(super) const MAX_DISCARD_SECTORS: u32 = 4 * 1024 * 1024;

/// Sectors per WRITE_ZEROES segment (8 MiB). Each segment costs a
/// bounded loop of `pwrite`, so keep one request short enough that it
/// does not hold an I/O worker for long.
pub(super) const MAX_WRITE_ZEROES_SECTORS: u32 = 16 * 1024;

/// The only flag defined for WRITE_ZEROES. It is not legal on DISCARD.
const FLAG_UNMAP: u32 = 1 << 0;

/// Bytes written per `pwrite` in the zero loop.
const ZERO_CHUNK: usize = 128 * 1024;

/// Copy the request payload out of guest memory.
///
/// The payload is copied because a vCPU can rewrite guest memory after
/// any check made against it in place.
fn read_payload(chain: &[ChainBuf], physmap: &PhysMap) -> Option<Vec<u8>> {
    let max_bytes = SEG_SIZE.checked_mul(MAX_SEG as usize)?;
    let mut out: Vec<u8> = Vec::new();
    for buf in &chain[1..chain.len() - 1] {
        // A device-writable data buffer has no meaning here, and reading
        // one would let the guest name a range it never supplied.
        let ChainBuf::Readable { addr, len } = buf else {
            return None;
        };
        let n = *len as usize;
        if out.len().checked_add(n)? > max_bytes {
            return None;
        }
        let sub = physmap.lookup(*addr, n)?;
        let base = out.len();
        out.resize(base + n, 0);
        sub.read_bytes(&mut out[base..]).ok()?;
    }
    Some(out)
}

/// Run one DISCARD or WRITE_ZEROES request.
///
/// Returns `(status, used_ring_len)`.
pub(super) fn execute(
    fd: i32,
    limits: &DiskLimits,
    rtype: u32,
    chain: &[ChainBuf],
    physmap: &PhysMap,
) -> (u8, u32) {
    let write_zeroes = rtype == bits::VIRTIO_BLK_T_WRITE_ZEROES;

    if limits.read_only {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if !write_zeroes && limits.nodelete {
        return (bits::VIRTIO_BLK_S_UNSUPP, 1);
    }
    // Header, at least one payload buffer, status byte.
    if chain.len() < 3 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }

    let Some(payload) = read_payload(chain, physmap) else {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    };
    if payload.is_empty() || !payload.len().is_multiple_of(SEG_SIZE) {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }

    let max_sectors = if write_zeroes {
        MAX_WRITE_ZEROES_SECTORS
    } else {
        MAX_DISCARD_SECTORS
    };

    // The length check above leaves no remainder.
    let (segments, _) = payload.as_chunks::<SEG_SIZE>();
    let mut ranges: Vec<(u64, u32)> = Vec::with_capacity(segments.len());
    for raw in segments {
        let sector =
            u64::from_le_bytes(raw[..8].try_into().expect("8 byte field"));
        let num_sectors =
            u32::from_le_bytes(raw[8..12].try_into().expect("4 byte field"));
        let flags =
            u32::from_le_bytes(raw[12..16].try_into().expect("4 byte field"));

        // UNMAP is defined for WRITE_ZEROES only and every other bit is
        // reserved. Reporting OK for a flag we do not implement tells the
        // guest that something happened which did not.
        let allowed = if write_zeroes { FLAG_UNMAP } else { 0 };
        if flags & !allowed != 0 {
            return (bits::VIRTIO_BLK_S_UNSUPP, 1);
        }
        if num_sectors > max_sectors {
            return (bits::VIRTIO_BLK_S_IOERR, 1);
        }
        if sector
            .checked_add(u64::from(num_sectors))
            .is_none_or(|end| end > limits.capacity)
        {
            return (bits::VIRTIO_BLK_S_IOERR, 1);
        }
        ranges.push((sector, num_sectors));
    }

    // The data in a discarded range is unspecified (VirtIO 1.3, 5.2.6.2),
    // so a validated DISCARD is complete here. WRITE_ZEROES must read
    // back as zeros, so it always writes. UNMAP only permits the device
    // to deallocate as well; it never relaxes the zero guarantee.
    if !write_zeroes {
        return (bits::VIRTIO_BLK_S_OK, 1);
    }

    for (sector, num_sectors) in ranges {
        if !write_zero_range(fd, sector, num_sectors) {
            return (bits::VIRTIO_BLK_S_IOERR, 1);
        }
    }
    (bits::VIRTIO_BLK_S_OK, 1)
}

/// Write zeros over `num_sectors` sectors starting at `sector`.
///
/// illumos has no `fallocate(2)`. `fcntl(F_FREESP)` covers regular files
/// only and a zvol needs a hand-built `DKIOCFREE` list, so the answer
/// that is correct for both backing stores is a bounded loop of
/// `pwrite`. The caller has already bounded the range.
fn write_zero_range(fd: i32, sector: u64, num_sectors: u32) -> bool {
    let Some(mut offset) = sector.checked_mul(bits::SECTOR_SIZE) else {
        return false;
    };
    let Some(mut remaining) =
        u64::from(num_sectors).checked_mul(bits::SECTOR_SIZE)
    else {
        return false;
    };

    let zeros = vec![0u8; ZERO_CHUNK];
    while remaining > 0 {
        let chunk = remaining.min(ZERO_CHUNK as u64) as usize;
        let Ok(off) = libc::off_t::try_from(offset) else {
            return false;
        };
        // SAFETY: `zeros` holds at least `chunk` readable bytes and the
        // caller keeps `fd` open across the call.
        let n = unsafe { libc::pwrite(fd, zeros.as_ptr().cast(), chunk, off) };
        if n < 0 {
            if std::io::Error::last_os_error().kind()
                == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            return false;
        }
        // A zero-length result on a non-empty write would spin forever.
        let done = n as u64;
        if done == 0 {
            return false;
        }
        let Some(next) = offset.checked_add(done) else {
            return false;
        };
        offset = next;
        remaining -= done;
    }
    true
}
