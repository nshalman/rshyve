// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Zero-copy scatter-gather I/O between a virtio-blk chain and the disk.
//!
//! preadv/pwritev run straight against guest memory, so one syscall
//! serves a request whatever its segment count.

use vmm_core::mem::{PhysMap, SubMapping};

use super::{bits, ChainBuf, VirtioBlkReqHdr, BLK_REQ_HEADER_SIZE};

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

/// Maximum segments handled on the stack to avoid heap allocation.
/// Typical virtio-blk chains have 1-4 data segments; 16 covers all
/// common cases. Chains longer than this spill to a Vec.
const STACK_IOV_MAX: usize = 16;

/// Resolve chain data segments into (SubMapping, iovec) pairs.
///
/// `extractor` selects which ChainBuf variant to handle (Writable for
/// reads, Readable for writes) and returns `(addr, len)` on match.
///
/// Returns `(total_bytes, iov_count)` on success, or `None` on lookup failure.
/// The caller must keep `subs` alive for the duration of the syscall.
fn build_iovecs<F>(
    data_bufs: &[ChainBuf],
    physmap: &PhysMap,
    subs: &mut [Option<SubMapping>],
    iovs: &mut [libc::iovec],
    extractor: F,
) -> Option<(usize, usize)>
where
    F: Fn(&ChainBuf) -> Option<(u64, u32)>,
{
    let mut total_bytes = 0usize;
    let mut iov_count = 0usize;
    for (i, buf) in data_bufs.iter().enumerate() {
        if let Some((addr, len)) = extractor(buf) {
            let n = len as usize;
            let sub = physmap.lookup(addr, n)?;
            let ptr = unsafe { sub.as_ptr() };
            iovs[iov_count] = libc::iovec {
                iov_base: ptr as *mut libc::c_void,
                iov_len: n,
            };
            subs[i] = Some(sub);
            total_bytes += n;
            iov_count += 1;
        }
    }
    Some((total_bytes, iov_count))
}

/// Zero-copy preadv (scatter-gather) directly into guest memory.
/// One syscall regardless of segment count.
/// Returns (status, bytes_written) where bytes_written includes the status byte.
pub(super) fn do_pread(
    fd: i32,
    sector: u64,
    chain: &[ChainBuf],
    physmap: &PhysMap,
) -> (u8, u32) {
    let offset = match sector.checked_mul(bits::SECTOR_SIZE) {
        Some(o) => o as i64,
        None => return (bits::VIRTIO_BLK_S_IOERR, 1),
    };
    if offset < 0 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if chain.len() < 3 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }

    let data_bufs = &chain[1..chain.len() - 1];
    let nseg = data_bufs.len();

    // Stack path for typical small chains; heap fallback for large ones.
    let mut stack_subs: [Option<SubMapping>; STACK_IOV_MAX] =
        std::array::from_fn(|_| None);
    let mut stack_iovs: [libc::iovec; STACK_IOV_MAX] = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; STACK_IOV_MAX];

    let mut heap_subs: Vec<Option<SubMapping>> = Vec::new();
    let mut heap_iovs: Vec<libc::iovec> = Vec::new();

    let (subs, iovs): (&mut [Option<SubMapping>], &mut [libc::iovec]) =
        if nseg <= STACK_IOV_MAX {
            (&mut stack_subs[..nseg], &mut stack_iovs[..nseg])
        } else {
            heap_subs.resize_with(nseg, || None);
            heap_iovs.resize(
                nseg,
                libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                },
            );
            (&mut heap_subs, &mut heap_iovs)
        };

    let (total_bytes, iov_count) =
        match build_iovecs(data_bufs, physmap, subs, iovs, |buf| match buf {
            ChainBuf::Writable { addr, len } => Some((*addr, *len)),
            _ => None,
        }) {
            Some(v) => v,
            None => return (bits::VIRTIO_BLK_S_IOERR, 1),
        };

    if total_bytes == 0 {
        return (bits::VIRTIO_BLK_S_OK, 1);
    }

    let nread = unsafe {
        libc::preadv(
            fd,
            iovs.as_ptr(),
            iov_count as libc::c_int,
            offset as libc::off_t,
        )
    };
    // SubMappings in `subs` keep guest pages pinned until after preadv returns.
    if nread < 0 || nread as usize != total_bytes {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    let len = u32::try_from(total_bytes)
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    (bits::VIRTIO_BLK_S_OK, len) // +1 for status byte
}

/// Zero-copy pwritev (scatter-gather) directly from guest memory.
/// One syscall regardless of segment count.
/// Returns (status, bytes_written) where bytes_written is 1 (status byte only).
pub(super) fn do_pwrite(
    fd: i32,
    sector: u64,
    chain: &[ChainBuf],
    physmap: &PhysMap,
) -> (u8, u32) {
    let offset = match sector.checked_mul(bits::SECTOR_SIZE) {
        Some(o) => o as i64,
        None => return (bits::VIRTIO_BLK_S_IOERR, 1),
    };
    if offset < 0 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if chain.len() < 3 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }

    let data_bufs = &chain[1..chain.len() - 1];
    let nseg = data_bufs.len();

    let mut stack_subs: [Option<SubMapping>; STACK_IOV_MAX] =
        std::array::from_fn(|_| None);
    let mut stack_iovs: [libc::iovec; STACK_IOV_MAX] = [libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    }; STACK_IOV_MAX];

    let mut heap_subs: Vec<Option<SubMapping>> = Vec::new();
    let mut heap_iovs: Vec<libc::iovec> = Vec::new();

    let (subs, iovs): (&mut [Option<SubMapping>], &mut [libc::iovec]) =
        if nseg <= STACK_IOV_MAX {
            (&mut stack_subs[..nseg], &mut stack_iovs[..nseg])
        } else {
            heap_subs.resize_with(nseg, || None);
            heap_iovs.resize(
                nseg,
                libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                },
            );
            (&mut heap_subs, &mut heap_iovs)
        };

    let (total_bytes, iov_count) =
        match build_iovecs(data_bufs, physmap, subs, iovs, |buf| match buf {
            ChainBuf::Readable { addr, len } => Some((*addr, *len)),
            _ => None,
        }) {
            Some(v) => v,
            None => return (bits::VIRTIO_BLK_S_IOERR, 1),
        };

    if total_bytes == 0 {
        return (bits::VIRTIO_BLK_S_OK, 1);
    }

    let nwritten = unsafe {
        libc::pwritev(
            fd,
            iovs.as_ptr(),
            iov_count as libc::c_int,
            offset as libc::off_t,
        )
    };
    // SubMappings in `subs` keep guest pages pinned until after pwritev returns.
    if nwritten < 0 || nwritten as usize != total_bytes {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    (bits::VIRTIO_BLK_S_OK, 1) // writes only produce the status byte for guest
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
