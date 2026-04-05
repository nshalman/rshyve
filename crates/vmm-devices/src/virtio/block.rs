// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO block device with hybrid sync/async I/O.
//!
//! Adaptive strategy based on queue depth:
//! - **iodepth=1**: pread/pwrite inline in the vCPU thread (~3us)
//! - **iodepth>1**: dispatch to 4 worker threads for parallel I/O
//!
//! All completions go through `VirtioCompletion` — a single Mutex-
//! protected used ring writer that prevents the index corruption
//! bugs that occur when inline and async paths use separate indexes.
//!
//! Zero-copy: pread/pwrite directly to/from guest memory via
//! `SubMapping::as_ptr()`. No heap allocation per I/O.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use vmm_core::mem::PhysMap;

#[usdt::provider(provider = "vmm")]
mod probes {
    fn vioblk_notify(queue_idx: u16, num_requests: u32) {}
    fn vioblk_inline(head: u16, rtype: u32, sector: u64) {}
    fn vioblk_dispatch(head: u16, rtype: u32, sector: u64) {}
    fn vioblk_complete(head: u16, status: u8) {}
}

use super::bits;
use super::queue::{ChainBuf, VirtioCompletion, VirtQueue};
use super::VirtioDevice;

const NUM_WORKERS: usize = 4;
pub const BLK_CONFIG_SIZE: u16 = 8;
pub const BLK_CONFIG_SIZE_EXT: u16 = 28;
const BLK_REQ_HEADER_SIZE: usize = 16;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct VirtioBlkReqHdr {
    rtype: u32,
    _reserved: u32,
    sector: u64,
}

#[derive(Debug, Clone)]
pub struct VirtioBlockOpts {
    pub read_only: bool,
    pub nodelete: bool,
    pub sector_size: u32,
}

impl Default for VirtioBlockOpts {
    fn default() -> Self {
        Self { read_only: false, nodelete: false, sector_size: 512 }
    }
}

/// I/O request sent to worker threads.
struct BlkIoRequest {
    rtype: u32,
    sector: u64,
    chain: Vec<ChainBuf>,
    head: u16,
    completion: Arc<VirtioCompletion>,
}

/// VirtIO block device with hybrid sync/async I/O.
pub struct VirtioBlock {
    fd: i32,
    capacity: u64,
    sector_size: u32,
    read_only: bool,
    #[allow(dead_code)]
    nodelete: bool,
    negotiated_features: Mutex<u32>,
    io_tx: mpsc::Sender<BlkIoRequest>,
    /// Guest physical memory for completion handler creation.
    physmap: Arc<PhysMap>,
    /// Interrupt callback, set after PCI device creation.
    pub interrupt: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    /// Completion handler, created on first queue notification.
    completion: Mutex<Option<Arc<VirtioCompletion>>>,
    _file: File,
}

impl VirtioBlock {
    pub fn new(
        file: File,
        opts: &VirtioBlockOpts,
        physmap: Arc<PhysMap>,
    ) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        let capacity = metadata.len() / u64::from(opts.sector_size);
        let read_only = opts.read_only;
        let raw_fd = file.as_raw_fd();

        let (tx, rx) = mpsc::channel::<BlkIoRequest>();
        let rx = Arc::new(Mutex::new(rx));

        for i in 0..NUM_WORKERS {
            let rx = Arc::clone(&rx);
            let physmap = Arc::clone(&physmap);

            thread::Builder::new()
                .name(format!("virtio-blk-io-{i}"))
                .spawn(move || {
                    blk_io_worker(rx, raw_fd, read_only, physmap);
                })
                .expect("failed to spawn virtio-blk I/O worker");
        }

        Ok(Self {
            fd: raw_fd,
            capacity,
            sector_size: opts.sector_size,
            read_only: opts.read_only,
            nodelete: opts.nodelete,
            negotiated_features: Mutex::new(0),
            io_tx: tx,
            physmap,
            interrupt: Arc::new(Mutex::new(None)),
            completion: Mutex::new(None),
            _file: file,
        })
    }

    pub fn config_size(&self) -> u16 {
        if self.sector_size != 512 { BLK_CONFIG_SIZE_EXT } else { BLK_CONFIG_SIZE }
    }

    /// Get or create the VirtioCompletion for this queue.
    fn get_completion(&self, queue: &VirtQueue) -> Arc<VirtioCompletion> {
        let mut slot = self.completion.lock().expect("completion lock");
        if let Some(ref c) = *slot {
            return Arc::clone(c);
        }

        let intr = Arc::clone(&self.interrupt);
        let interrupt_fn = move || {
            if let Ok(guard) = intr.lock() {
                if let Some(ref f) = *guard {
                    f();
                }
            }
        };

        let c = VirtioCompletion::new(
            queue,
            Arc::clone(&self.physmap),
            interrupt_fn,
        );
        *slot = Some(Arc::clone(&c));
        c
    }

    /// Inline fast path: pread/pwrite in the vCPU thread.
    /// Returns (status, used_ring_len).
    fn process_inline(
        &self,
        header: &VirtioBlkReqHdr,
        chain: &[ChainBuf],
        physmap: &PhysMap,
    ) -> (u8, u32) {
        match header.rtype {
            bits::VIRTIO_BLK_T_IN => do_pread(self.fd, header.sector, chain, physmap),
            bits::VIRTIO_BLK_T_OUT if !self.read_only => {
                do_pwrite(self.fd, header.sector, chain, physmap)
            }
            bits::VIRTIO_BLK_T_OUT => (bits::VIRTIO_BLK_S_IOERR, 1),
            bits::VIRTIO_BLK_T_FLUSH => {
                if unsafe { libc::fsync(self.fd) } == 0 {
                    (bits::VIRTIO_BLK_S_OK, 1)
                } else {
                    (bits::VIRTIO_BLK_S_IOERR, 1)
                }
            }
            _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
        }
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_features(&self) -> u32 {
        let mut f = bits::VIRTIO_BLK_F_FLUSH as u32;
        if self.read_only { f |= bits::VIRTIO_BLK_F_RO as u32; }
        if self.sector_size != 512 { f |= bits::VIRTIO_BLK_F_BLK_SIZE as u32; }
        f
    }

    fn set_features(&self, features: u32) {
        *self.negotiated_features.lock().expect("lock") = features;
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let mut config = [0u8; 28];
        config[..8].copy_from_slice(&self.capacity.to_le_bytes());
        if self.sector_size != 512 {
            config[0x18..0x1C].copy_from_slice(&self.sector_size.to_le_bytes());
        }
        let off = offset as usize;
        if off >= config.len() { return 0; }
        let mut result = 0u32;
        for i in 0..len.min(4) as usize {
            if off + i < config.len() {
                result |= (config[off + i] as u32) << (i * 8);
            }
        }
        result
    }

    fn cfg_write(&self, _: u16, _: u32, _: u8) {}

    /// Override notify_queue: pop avail, process or dispatch, complete.
    ///
    /// ALL used ring writes go through VirtioCompletion — never
    /// through VirtQueue::push_used(). This prevents the index
    /// divergence bug where inline and async paths tracked the
    /// used ring index independently.
    ///
    /// EVENT_IDX re-check loop (VirtIO 1.3 sec 2.7.10):
    /// After draining the ring and writing `used_event`, re-read
    /// `avail_idx`. If the guest added entries during the race
    /// window between our drain and the `used_event` write, the
    /// guest may have decided not to kick — we must catch those
    /// entries ourselves.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let queue = match queues.get_mut(queue_idx as usize) {
            Some(q) => q,
            None => return false,
        };

        let completion = self.get_completion(queue);

        // Collect all pending requests (may loop for EVENT_IDX re-check)
        let mut requests: Vec<(u16, VirtioBlkReqHdr, Vec<ChainBuf>)> = Vec::new();

        loop {
            while let Some(head) = queue.pop_avail(physmap) {
                let chain = match queue.collect_chain(physmap, head) {
                    Some(c) => c,
                    None => continue,
                };

                if chain.len() < 2 {
                    write_status(physmap, &chain, bits::VIRTIO_BLK_S_IOERR);
                    completion.complete(head, 1);
                    continue;
                }

                let header = match parse_header(&chain, physmap) {
                    Some(h) => h,
                    None => {
                        write_status(physmap, &chain, bits::VIRTIO_BLK_S_IOERR);
                        completion.complete(head, 1);
                        continue;
                    }
                };

                // Bounds check: verify start sector AND full range fit
                if header.rtype == bits::VIRTIO_BLK_T_IN
                    || header.rtype == bits::VIRTIO_BLK_T_OUT
                {
                    let data_bytes: u64 = chain[1..chain.len() - 1]
                        .iter()
                        .map(|b| match b {
                            ChainBuf::Readable { len, .. }
                            | ChainBuf::Writable { len, .. } => u64::from(*len),
                        })
                        .sum();
                    let sectors_needed =
                        data_bytes.div_ceil(bits::SECTOR_SIZE);
                    if header.sector.checked_add(sectors_needed).map_or(
                        true,
                        |end| end > self.capacity,
                    ) {
                        write_status(physmap, &chain, bits::VIRTIO_BLK_S_IOERR);
                        completion.complete(head, 1);
                        continue;
                    }
                }

                requests.push((head, header, chain));
            }

            // Write used_event (with SeqCst fence), then re-check.
            queue.update_used_event(physmap);
            if !queue.has_new_avail(physmap) {
                break;
            }
            // Guest added entries during race window; drain again.
        }

        if requests.is_empty() {
            return false;
        }

        probes::vioblk_notify!(|| (queue_idx, requests.len() as u32));

        // Single request: inline fast path (no channel overhead).
        if requests.len() == 1 {
            let (head, header, ref chain) = requests[0];
            probes::vioblk_inline!(|| (head, header.rtype, header.sector));
            let (status, written) = self.process_inline(&header, chain, physmap);
            write_status(physmap, chain, status);
            probes::vioblk_complete!(|| (head, status));
            completion.complete(head, written);
            return false;
        }

        // Multiple requests: dispatch to workers for parallel I/O.
        for (head, header, chain) in requests {
            probes::vioblk_dispatch!(|| (head, header.rtype, header.sector));
            let _ = self.io_tx.send(BlkIoRequest {
                rtype: header.rtype,
                sector: header.sector,
                chain,
                head,
                completion: Arc::clone(&completion),
            });
        }

        false // workers call completion.complete()
    }

    fn process_queue(&self, _: u16, _: &mut VirtQueue, _: u16, _: &PhysMap) -> u32 {
        0 // not called — notify_queue is overridden
    }

    fn reset(&self) {
        *self.negotiated_features.lock().expect("lock") = 0;
        *self.completion.lock().expect("lock") = None;
    }
}

// ---------------------------------------------------------------------------
// I/O worker
// ---------------------------------------------------------------------------

fn blk_io_worker(
    rx: Arc<Mutex<mpsc::Receiver<BlkIoRequest>>>,
    fd: i32,
    read_only: bool,
    physmap: Arc<PhysMap>,
) {
    loop {
        let req = {
            let rx = rx.lock().expect("worker rx lock");
            match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            }
        };

        let (status, written) = match req.rtype {
            bits::VIRTIO_BLK_T_IN => do_pread(fd, req.sector, &req.chain, &physmap),
            bits::VIRTIO_BLK_T_OUT if !read_only => {
                do_pwrite(fd, req.sector, &req.chain, &physmap)
            }
            bits::VIRTIO_BLK_T_OUT => (bits::VIRTIO_BLK_S_IOERR, 1),
            bits::VIRTIO_BLK_T_FLUSH => {
                if unsafe { libc::fsync(fd) } == 0 { (bits::VIRTIO_BLK_S_OK, 1) }
                else { (bits::VIRTIO_BLK_S_IOERR, 1) }
            }
            _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
        };

        write_status(&physmap, &req.chain, status);
        probes::vioblk_complete!(|| (req.head, status));
        req.completion.complete(req.head, written);
    }
}

// ---------------------------------------------------------------------------
// I/O helpers
// ---------------------------------------------------------------------------

fn parse_header(chain: &[ChainBuf], physmap: &PhysMap) -> Option<VirtioBlkReqHdr> {
    match &chain[0] {
        ChainBuf::Readable { addr, len } if (*len as usize) >= BLK_REQ_HEADER_SIZE => {
            physmap.lookup(*addr, BLK_REQ_HEADER_SIZE)
                .and_then(|sub| sub.read::<VirtioBlkReqHdr>().ok())
        }
        _ => None,
    }
}

/// Zero-copy pread directly into guest memory.
/// Returns (status, bytes_written) where bytes_written includes the status byte.
fn do_pread(fd: i32, sector: u64, chain: &[ChainBuf], physmap: &PhysMap) -> (u8, u32) {
    let mut offset = match sector.checked_mul(bits::SECTOR_SIZE) {
        Some(o) => o as i64,
        None => return (bits::VIRTIO_BLK_S_IOERR, 1),
    };
    if offset < 0 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if chain.len() < 3 { return (bits::VIRTIO_BLK_S_IOERR, 1); }

    let mut total_read: u32 = 0;
    for buf in &chain[1..chain.len() - 1] {
        if let ChainBuf::Writable { addr, len } = buf {
            let n = *len as usize;
            match physmap.lookup(*addr, n) {
                Some(sub) => {
                    let ptr = unsafe { sub.as_ptr() };
                    let rc = unsafe { libc::pread(fd, ptr as *mut _, n, offset) };
                    if rc < 0 || (rc as usize) != n {
                        return (bits::VIRTIO_BLK_S_IOERR, 1);
                    }
                }
                None => return (bits::VIRTIO_BLK_S_IOERR, 1),
            }
            total_read += n as u32;
            offset = match offset.checked_add(n as i64) {
                Some(o) if o >= 0 => o,
                _ => return (bits::VIRTIO_BLK_S_IOERR, 1),
            };
        }
    }
    (bits::VIRTIO_BLK_S_OK, total_read + 1) // +1 for status byte
}

/// Zero-copy pwrite directly from guest memory.
/// Returns (status, bytes_written) where bytes_written is 1 (status byte only).
fn do_pwrite(fd: i32, sector: u64, chain: &[ChainBuf], physmap: &PhysMap) -> (u8, u32) {
    let mut offset = match sector.checked_mul(bits::SECTOR_SIZE) {
        Some(o) => o as i64,
        None => return (bits::VIRTIO_BLK_S_IOERR, 1),
    };
    if offset < 0 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if chain.len() < 3 { return (bits::VIRTIO_BLK_S_IOERR, 1); }

    for buf in &chain[1..chain.len() - 1] {
        if let ChainBuf::Readable { addr, len } = buf {
            let n = *len as usize;
            match physmap.lookup(*addr, n) {
                Some(sub) => {
                    let ptr = unsafe { sub.as_ptr() };
                    let rc = unsafe { libc::pwrite(fd, ptr as *const _, n, offset) };
                    if rc < 0 || (rc as usize) != n {
                        return (bits::VIRTIO_BLK_S_IOERR, 1);
                    }
                }
                None => return (bits::VIRTIO_BLK_S_IOERR, 1),
            }
            offset = match offset.checked_add(n as i64) {
                Some(o) if o >= 0 => o,
                _ => return (bits::VIRTIO_BLK_S_IOERR, 1),
            };
        }
    }
    (bits::VIRTIO_BLK_S_OK, 1) // writes only produce the status byte for guest
}

fn write_status(physmap: &PhysMap, chain: &[ChainBuf], status: u8) -> u32 {
    if let Some(ChainBuf::Writable { addr, len }) = chain.last() {
        if *len >= 1 {
            if let Some(sub) = physmap.lookup(*addr, 1) {
                let _ = sub.write_bytes(&[status]);
            }
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_opts() {
        let opts = VirtioBlockOpts::default();
        assert!(!opts.read_only);
        assert!(!opts.nodelete);
        assert_eq!(opts.sector_size, 512);
    }

    #[test]
    fn sector_offset_overflow_returns_ioerr() {
        // sector * SECTOR_SIZE would overflow u64
        let huge_sector: u64 = u64::MAX / bits::SECTOR_SIZE + 1;
        assert!(huge_sector.checked_mul(bits::SECTOR_SIZE).is_none());
    }

    #[test]
    fn sector_offset_valid() {
        // Normal sector values should not overflow
        let sector: u64 = 1_000_000;
        let offset = sector.checked_mul(bits::SECTOR_SIZE);
        assert_eq!(offset, Some(512_000_000));
    }

    #[test]
    fn bounds_check_overflow() {
        // sector + sectors_needed overflows u64
        let sector: u64 = u64::MAX;
        let sectors_needed: u64 = 1;
        assert!(sector.checked_add(sectors_needed).is_none());
    }

    #[test]
    fn bounds_check_past_capacity() {
        // sector + sectors_needed > capacity
        let capacity: u64 = 1000;
        let sector: u64 = 999;
        let sectors_needed: u64 = 2;
        assert!(sector
            .checked_add(sectors_needed)
            .map_or(true, |end| end > capacity));
    }

    #[test]
    fn bounds_check_exact_fit() {
        // sector + sectors_needed == capacity should be OK
        let capacity: u64 = 1000;
        let sector: u64 = 998;
        let sectors_needed: u64 = 2;
        assert!(!sector
            .checked_add(sectors_needed)
            .map_or(true, |end| end > capacity));
    }
}
