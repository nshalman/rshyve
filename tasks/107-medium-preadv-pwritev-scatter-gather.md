# PERF: Use preadv/pwritev for scatter-gather I/O

**Priority:** Medium
**File:** `crates/vmm-devices/src/virtio/block.rs` lines 372-417

## Problem

`do_pread()` and `do_pwrite()` issue one `pread`/`pwrite` syscall per buffer segment.
A multi-segment chain (e.g., 4KB split into 512-byte descriptors) makes 8 syscalls.
`preadv`/`pwritev` batches these into 1 syscall.

The NVMe device at `crates/vmm-devices/src/nvme/mod.rs` (lines ~1259-1506) already uses
this exact pattern with `iovec` arrays.

## Fix

Replace `do_pread()` and `do_pwrite()` with vectored I/O:

```rust
/// Build iovec array from chain data buffers, keeping SubMappings alive.
fn chain_to_iovecs(
    chain: &[ChainBuf],
    physmap: &PhysMap,
    want_writable: bool,
) -> Option<(Vec<libc::iovec>, Vec<SubMapping>)> {
    let data_bufs = &chain[1..chain.len() - 1];
    let mut iovecs = Vec::with_capacity(data_bufs.len());
    let mut subs = Vec::with_capacity(data_bufs.len());

    for buf in data_bufs {
        let (addr, len, is_writable) = match buf {
            ChainBuf::Writable { addr, len } => (*addr, *len as usize, true),
            ChainBuf::Readable { addr, len } => (*addr, *len as usize, false),
        };
        if is_writable != want_writable {
            continue;
        }
        let sub = physmap.lookup(addr, len)?;
        iovecs.push(libc::iovec {
            iov_base: unsafe { sub.as_ptr() } as *mut libc::c_void,
            iov_len: len,
        });
        subs.push(sub);
    }

    if iovecs.is_empty() { return None; }
    Some((iovecs, subs))
}

fn do_pread(fd: i32, sector: u64, chain: &[ChainBuf], physmap: &PhysMap) -> u8 {
    if chain.len() < 3 { return bits::VIRTIO_BLK_S_IOERR; }
    let offset = (sector * bits::SECTOR_SIZE) as libc::off_t;

    let (iovecs, _subs) = match chain_to_iovecs(chain, physmap, true) {
        Some(v) => v,
        None => return bits::VIRTIO_BLK_S_IOERR,
    };

    let total: usize = iovecs.iter().map(|iov| iov.iov_len).sum();
    let rc = unsafe {
        libc::preadv(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int, offset)
    };
    // _subs keeps SubMappings alive until here

    if rc < 0 || (rc as usize) != total {
        bits::VIRTIO_BLK_S_IOERR
    } else {
        bits::VIRTIO_BLK_S_OK
    }
}

fn do_pwrite(fd: i32, sector: u64, chain: &[ChainBuf], physmap: &PhysMap) -> u8 {
    if chain.len() < 3 { return bits::VIRTIO_BLK_S_IOERR; }
    let offset = (sector * bits::SECTOR_SIZE) as libc::off_t;

    let (iovecs, _subs) = match chain_to_iovecs(chain, physmap, false) {
        Some(v) => v,
        None => return bits::VIRTIO_BLK_S_IOERR,
    };

    let total: usize = iovecs.iter().map(|iov| iov.iov_len).sum();
    let rc = unsafe {
        libc::pwritev(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int, offset)
    };

    if rc < 0 || (rc as usize) != total {
        bits::VIRTIO_BLK_S_IOERR
    } else {
        bits::VIRTIO_BLK_S_OK
    }
}
```

## Verification

- `cargo test -p vmm_devices` passes
- `fio --rw=read --verify=crc32` and `--rw=write --verify=crc32` correct data
- dtrace `syscall::preadv:entry` confirms single syscall per I/O
- Benchmark: compare latency for 4KB random reads before/after
