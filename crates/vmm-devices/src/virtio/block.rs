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
//! Zero-copy: preadv/pwritev scatter-gather I/O directly to/from
//! guest memory via `SubMapping::as_ptr()`. One syscall per request
//! regardless of segment count. Stack-allocated iovecs for <= 16
//! segments to avoid heap allocation on the hot path.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::blkdev::DiskCache;
use crate::lifecycle::Indicator;
use crate::quiesce::datasync;
use crate::{FlushError, FlushIntent, Lifecycle, QuiesceGate};

use vmm_core::mem::{PhysMap, SubMapping};

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

/// Default I/O workers for a single-queue virtio-blk device.
///
/// Matches propolis's default (`propolis-server/src/lib/initializer.rs:61`).
/// bhyve's 16 is a shared pool across all disks; ours and propolis's are
/// per-disk. 8 is enough parallel syscalls for deep queues without hotspotting
/// the `VirtioCompletion` Mutex<u16>.
const NUM_WORKERS: usize = 8;
pub const BLK_CONFIG_SIZE: u16 = 8;
/// Extended config size covering all VirtIO 1.3 block config fields
/// (capacity through write_zeroes_may_unmap at offset 0x38+1=0x3C).
/// Must match the cfg_read buffer size so the modern driver's BUG_ON
/// (offset+len > device_len) doesn't fire.
pub const BLK_CONFIG_SIZE_EXT: u16 = 60;
const BLK_REQ_HEADER_SIZE: usize = 16;

/// Maximum size of any single segment (128K).
const SIZE_MAX: u32 = 128 * 1024;
/// Maximum number of segments in a single request.
const SEG_MAX: u32 = 126;
/// VirtIO block device serial ID length.
const VIRTIO_BLK_ID_BYTES: usize = 20;

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
    /// Number of virtqueues. `None` means "caller decides" (typically
    /// `min(num_vcpus, 4)`). Capped at 8 — a zvol has one SPA sync pipeline
    /// so more queues don't add throughput but do add worker threads.
    pub num_queues: Option<u16>,
}

impl Default for VirtioBlockOpts {
    fn default() -> Self {
        Self {
            read_only: false,
            nodelete: false,
            sector_size: 512,
            num_queues: None,
        }
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
    /// Number of request queues (1 = single queue, >1 = multiqueue).
    num_queues: u16,
    /// Number of I/O worker threads per queue.
    workers_per_queue: usize,
    negotiated_features: Mutex<u64>,
    /// Per-worker I/O sender channels (flat: queue_idx * workers_per_queue + worker_idx).
    io_txs: Vec<mpsc::Sender<BlkIoRequest>>,
    /// Per-queue round-robin counter for dispatching to workers.
    next_worker: Vec<AtomicUsize>,
    /// Guest physical memory for completion handler creation.
    physmap: Arc<PhysMap>,
    /// Interrupt callback, set after PCI device creation.
    pub interrupt: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    /// Per-queue completion handlers, created on first notification.
    completions: Mutex<Vec<Option<Arc<VirtioCompletion>>>>,
    /// Zvol write-cache control. Declared BEFORE `_file` so Drop runs
    /// `DKIOCSETWCE` restore while the fd is still open.
    _cache: DiskCache,
    _file: File,
    /// Lifecycle state indicator.
    indicator: Indicator,
    gate: Arc<QuiesceGate>,
}

impl VirtioBlock {
    pub fn new(
        file: File,
        opts: &VirtioBlockOpts,
        physmap: Arc<PhysMap>,
        num_queues: u16,
    ) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        let capacity = metadata.len() / u64::from(opts.sector_size);
        let read_only = opts.read_only;
        let raw_fd = file.as_raw_fd();

        // Enable zvol writeback cache (DKIOCSETWCE=1). Must happen before
        // any I/O so the first guest writes already hit the fast path.
        let cache = DiskCache::new(raw_fd, read_only);

        // Workers per queue: NUM_WORKERS for single queue, 2 per queue for MQ.
        let workers_per_queue = if num_queues <= 1 { NUM_WORKERS } else { 2 };
        let total_workers = num_queues as usize * workers_per_queue;
        let gate = Arc::new(QuiesceGate::new(total_workers));
        let mut io_txs = Vec::with_capacity(total_workers);
        let mut next_worker = Vec::with_capacity(num_queues as usize);

        for q in 0..num_queues {
            for i in 0..workers_per_queue {
                let (tx, rx) = mpsc::channel::<BlkIoRequest>();
                let physmap = Arc::clone(&physmap);
                let gate = Arc::clone(&gate);

                thread::Builder::new()
                    .name(format!("virtio-blk-q{q}-io-{i}"))
                    .spawn(move || {
                        blk_io_worker(rx, raw_fd, read_only, physmap, gate);
                    })
                    .expect("failed to spawn virtio-blk I/O worker");

                io_txs.push(tx);
            }
            next_worker.push(AtomicUsize::new(0));
        }

        let completions = (0..num_queues as usize).map(|_| None).collect();

        Ok(Self {
            fd: raw_fd,
            capacity,
            sector_size: opts.sector_size,
            read_only: opts.read_only,
            nodelete: opts.nodelete,
            num_queues,
            workers_per_queue,
            negotiated_features: Mutex::new(0),
            io_txs,
            next_worker,
            physmap,
            interrupt: Arc::new(Mutex::new(None)),
            completions: Mutex::new(completions),
            _cache: cache,
            _file: file,
            indicator: Indicator::new(),
            gate,
        })
    }

    /// Number of request queues.
    pub fn num_queues(&self) -> u16 {
        self.num_queues
    }

    pub fn config_size(&self) -> u16 {
        BLK_CONFIG_SIZE_EXT
    }

    /// Return a device ID string for GET_ID requests.
    fn device_id(&self) -> [u8; VIRTIO_BLK_ID_BYTES] {
        let mut id = [0u8; VIRTIO_BLK_ID_BYTES];
        let s = b"vmm-virtio-blk";
        let n = s.len().min(VIRTIO_BLK_ID_BYTES);
        id[..n].copy_from_slice(&s[..n]);
        id
    }

    /// Get or create the VirtioCompletion for the given queue index.
    fn get_completion(&self, queue_idx: u16, queue: &VirtQueue) -> Arc<VirtioCompletion> {
        let mut slots = self.completions.lock().expect("completion lock");
        let idx = queue_idx as usize;
        if idx < slots.len() {
            if let Some(ref c) = slots[idx] {
                return Arc::clone(c);
            }
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
        // Force-interrupt the first 32 completions so the guest's
        // blk driver always gets notified after migration. Without
        // this, EVENT_IDX suppression can cause the guest to miss
        // completions and stall disk I/O.
        c.set_force_interrupt(32);
        if idx < slots.len() {
            slots[idx] = Some(Arc::clone(&c));
        }
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
                if datasync(self.fd) == 0 {
                    (bits::VIRTIO_BLK_S_OK, 1)
                } else {
                    (bits::VIRTIO_BLK_S_IOERR, 1)
                }
            }
            bits::VIRTIO_BLK_T_GET_ID => {
                let id = self.device_id();
                // Write ID to the first writable buffer in the chain
                for buf in chain {
                    if let ChainBuf::Writable { addr, len } = buf {
                        let n = (*len as usize).min(VIRTIO_BLK_ID_BYTES);
                        if let Some(sub) = physmap.lookup(*addr, n) {
                            // write to guest memory; no recovery if unmapped
                            let _ = sub.write_bytes(&id[..n]);
                        }
                        return (bits::VIRTIO_BLK_S_OK, n as u32 + 1);
                    }
                }
                (bits::VIRTIO_BLK_S_IOERR, 1)
            }
            bits::VIRTIO_BLK_T_DISCARD | bits::VIRTIO_BLK_T_WRITE_ZEROES => {
                // Accept but treat as no-op for now. The guest gets
                // success so it can use these hints; actual hole-
                // punching / zero-fill can be added later.
                (bits::VIRTIO_BLK_S_OK, 1)
            }
            _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
        }
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_features(&self) -> u64 {
        let mut f = bits::VIRTIO_BLK_F_FLUSH
            | bits::VIRTIO_BLK_F_SIZE_MAX
            | bits::VIRTIO_BLK_F_SEG_MAX;
        if self.read_only { f |= bits::VIRTIO_BLK_F_RO; }
        if self.sector_size != 512 { f |= bits::VIRTIO_BLK_F_BLK_SIZE; }
        if !self.read_only {
            f |= bits::VIRTIO_BLK_F_DISCARD
                | bits::VIRTIO_BLK_F_WRITE_ZEROES;
        }
        if self.num_queues > 1 {
            f |= bits::VIRTIO_BLK_F_MQ;
        }
        // Ring features: included in device_features so modern transport
        // advertises them. Legacy transport strips these in pci.rs
        // (EVENT_IDX causes deadlock with legacy on Linux 6.14).
        f |= bits::VIRTIO_F_RING_EVENT_IDX;
        f |= bits::VIRTIO_F_RING_INDIRECT_DESC;
        f
    }

    fn set_features(&self, features: u64) {
        *self.negotiated_features.lock().expect("lock") = features;
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let mut config = [0u8; 60];
        // capacity at offset 0x00 (8 bytes)
        config[..8].copy_from_slice(&self.capacity.to_le_bytes());
        // size_max at offset 0x08 (4 bytes)
        config[0x08..0x0C].copy_from_slice(&SIZE_MAX.to_le_bytes());
        // seg_max at offset 0x0C (4 bytes)
        config[0x0C..0x10].copy_from_slice(&SEG_MAX.to_le_bytes());
        // blk_size at offset 0x14 (4 bytes)
        config[0x14..0x18].copy_from_slice(&self.sector_size.to_le_bytes());
        // num_queues at offset 0x22 (2 bytes) — VirtIO block MQ config
        config[0x22..0x24].copy_from_slice(&self.num_queues.to_le_bytes());
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

        // Debug: log first few notify calls after migration
        static NOTIFY_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let count = NOTIFY_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count < 5 {
            let guest_avail = queue.read_avail_idx(physmap);
            eprintln!("[blk-notify] q={} last_avail={} guest_avail={} shadow_used={}",
                queue_idx, queue.last_avail_idx(), guest_avail, queue.shadow_used_idx());
        }

        let completion = self.get_completion(queue_idx, queue);

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

                // Bounds check: verify start sector AND full range fit.
                // Also reject zero-length data transfers — a guest can
                // craft a request with no data buffers to bypass the
                // sector range check (0 + 0 <= capacity is always true).
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
                    if data_bytes == 0 {
                        write_status(physmap, &chain, bits::VIRTIO_BLK_S_IOERR);
                        completion.complete(head, 1);
                        continue;
                    }
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

        // Small batch inline fast path: reads and metadata ops only.
        //
        // Writes and flushes always dispatch to workers — they may block
        // the thread for milliseconds on ZIL commit, and running inline
        // would stall the guest vCPU. Reads are near-zero-cost from the
        // ARC so keeping them inline preserves iodepth=1 read latency.
        let all_inline_safe = requests.iter().all(|(_, h, _)| {
            h.rtype != bits::VIRTIO_BLK_T_OUT
                && h.rtype != bits::VIRTIO_BLK_T_FLUSH
        });
        if requests.len() <= 4 && all_inline_safe {
            for &(head, ref header, ref chain) in &requests {
                probes::vioblk_inline!(|| (head, header.rtype, header.sector));
                let (status, written) = self.process_inline(header, chain, physmap);
                write_status(physmap, chain, status);
                probes::vioblk_complete!(|| (head, status));
                completion.complete(head, written);
            }
            return false;
        }

        // Multiple requests: round-robin dispatch to per-worker channels.
        let base = queue_idx as usize * self.workers_per_queue;
        for (head, header, chain) in requests {
            probes::vioblk_dispatch!(|| (head, header.rtype, header.sector));
            let idx = self.next_worker[queue_idx as usize]
                .fetch_add(1, Ordering::Relaxed)
                % self.workers_per_queue;
            // receiver dropped during shutdown; safe to ignore
            let _ = self.io_txs[base + idx].send(BlkIoRequest {
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
        let mut slots = self.completions.lock().expect("lock");
        for slot in slots.iter_mut() {
            *slot = None;
        }
    }
}

impl Lifecycle for VirtioBlock {
    fn type_name(&self) -> &'static str {
        "virtio-blk"
    }

    fn lifecycle_state(&self) -> Option<crate::lifecycle::IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
        self.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        self.gate.is_quiesced()
    }

    fn resume(&self) {
        self.gate.resume();
        self.indicator.resume();
    }

    fn halt(&self) {
        self.indicator.halt();
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        crate::quiesce::flush_file(&self.gate, &self._file, intent, "virtio-blk")
    }
}

// ---------------------------------------------------------------------------
// I/O worker
// ---------------------------------------------------------------------------

fn blk_io_worker(
    rx: mpsc::Receiver<BlkIoRequest>,
    fd: i32,
    read_only: bool,
    physmap: Arc<PhysMap>,
    gate: Arc<QuiesceGate>,
) {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        let req = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                gate.park_if_paused();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };

        // Process first request
        let completion = Arc::clone(&req.completion);
        let first = process_one_request(fd, read_only, &physmap, req);

        // Try to drain more pending requests for batching.
        // This reduces Mutex acquisitions in VirtioCompletion:
        // one complete_batch() instead of N complete() calls.
        let mut batch = Vec::new();
        batch.push(first);
        while batch.len() < 16 {
            match rx.try_recv() {
                Ok(req) => {
                    batch.push(process_one_request(fd, read_only, &physmap, req));
                }
                Err(_) => break,
            }
        }

        if batch.len() == 1 {
            let (head, written) = batch[0];
            completion.complete(head, written);
        } else {
            completion.complete_batch(&batch);
        }
    }
}

/// Process a single I/O request: execute the operation, write status.
/// Returns (head, written) for completion batching.
fn process_one_request(
    fd: i32,
    read_only: bool,
    physmap: &PhysMap,
    req: BlkIoRequest,
) -> (u16, u32) {
    let (status, written) = match req.rtype {
        bits::VIRTIO_BLK_T_IN => do_pread(fd, req.sector, &req.chain, physmap),
        bits::VIRTIO_BLK_T_OUT if !read_only => {
            do_pwrite(fd, req.sector, &req.chain, physmap)
        }
        bits::VIRTIO_BLK_T_OUT => (bits::VIRTIO_BLK_S_IOERR, 1),
        bits::VIRTIO_BLK_T_FLUSH => {
            if datasync(fd) == 0 { (bits::VIRTIO_BLK_S_OK, 1) }
            else { (bits::VIRTIO_BLK_S_IOERR, 1) }
        }
        bits::VIRTIO_BLK_T_DISCARD | bits::VIRTIO_BLK_T_WRITE_ZEROES => {
            (bits::VIRTIO_BLK_S_OK, 1)
        }
        _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
    };

    write_status(physmap, &req.chain, status);
    probes::vioblk_complete!(|| (req.head, status));
    (req.head, written)
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
fn do_pread(fd: i32, sector: u64, chain: &[ChainBuf], physmap: &PhysMap) -> (u8, u32) {
    let offset = match sector.checked_mul(bits::SECTOR_SIZE) {
        Some(o) => o as i64,
        None => return (bits::VIRTIO_BLK_S_IOERR, 1),
    };
    if offset < 0 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if chain.len() < 3 { return (bits::VIRTIO_BLK_S_IOERR, 1); }

    let data_bufs = &chain[1..chain.len() - 1];
    let nseg = data_bufs.len();

    // Stack path for typical small chains; heap fallback for large ones.
    let mut stack_subs: [Option<SubMapping>; STACK_IOV_MAX] =
        std::array::from_fn(|_| None);
    let mut stack_iovs: [libc::iovec; STACK_IOV_MAX] =
        [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; STACK_IOV_MAX];

    let mut heap_subs: Vec<Option<SubMapping>> = Vec::new();
    let mut heap_iovs: Vec<libc::iovec> = Vec::new();

    let (subs, iovs): (&mut [Option<SubMapping>], &mut [libc::iovec]) =
        if nseg <= STACK_IOV_MAX {
            (&mut stack_subs[..nseg], &mut stack_iovs[..nseg])
        } else {
            heap_subs.resize_with(nseg, || None);
            heap_iovs.resize(nseg, libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 });
            (&mut heap_subs, &mut heap_iovs)
        };

    let (total_bytes, iov_count) = match build_iovecs(
        data_bufs,
        physmap,
        subs,
        iovs,
        |buf| match buf {
            ChainBuf::Writable { addr, len } => Some((*addr, *len)),
            _ => None,
        },
    ) {
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
    let len = u32::try_from(total_bytes).unwrap_or(u32::MAX).saturating_add(1);
    (bits::VIRTIO_BLK_S_OK, len) // +1 for status byte
}

/// Zero-copy pwritev (scatter-gather) directly from guest memory.
/// One syscall regardless of segment count.
/// Returns (status, bytes_written) where bytes_written is 1 (status byte only).
fn do_pwrite(fd: i32, sector: u64, chain: &[ChainBuf], physmap: &PhysMap) -> (u8, u32) {
    let offset = match sector.checked_mul(bits::SECTOR_SIZE) {
        Some(o) => o as i64,
        None => return (bits::VIRTIO_BLK_S_IOERR, 1),
    };
    if offset < 0 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if chain.len() < 3 { return (bits::VIRTIO_BLK_S_IOERR, 1); }

    let data_bufs = &chain[1..chain.len() - 1];
    let nseg = data_bufs.len();

    let mut stack_subs: [Option<SubMapping>; STACK_IOV_MAX] =
        std::array::from_fn(|_| None);
    let mut stack_iovs: [libc::iovec; STACK_IOV_MAX] =
        [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; STACK_IOV_MAX];

    let mut heap_subs: Vec<Option<SubMapping>> = Vec::new();
    let mut heap_iovs: Vec<libc::iovec> = Vec::new();

    let (subs, iovs): (&mut [Option<SubMapping>], &mut [libc::iovec]) =
        if nseg <= STACK_IOV_MAX {
            (&mut stack_subs[..nseg], &mut stack_iovs[..nseg])
        } else {
            heap_subs.resize_with(nseg, || None);
            heap_iovs.resize(nseg, libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 });
            (&mut heap_subs, &mut heap_iovs)
        };

    let (total_bytes, iov_count) = match build_iovecs(
        data_bufs,
        physmap,
        subs,
        iovs,
        |buf| match buf {
            ChainBuf::Readable { addr, len } => Some((*addr, *len)),
            _ => None,
        },
    ) {
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

fn write_status(physmap: &PhysMap, chain: &[ChainBuf], status: u8) -> u32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiesce_query_is_non_blocking_with_active_worker() {
        let file = tempfile::tempfile().expect("create tempfile");
        let fd = file.as_raw_fd();
        let device: Arc<dyn Lifecycle> = Arc::new(VirtioBlock {
            fd,
            capacity: 0,
            sector_size: 512,
            read_only: true,
            nodelete: false,
            num_queues: 0,
            workers_per_queue: 0,
            negotiated_features: Mutex::new(0),
            io_txs: Vec::new(),
            next_worker: Vec::new(),
            physmap: Arc::new(PhysMap::new()),
            interrupt: Arc::new(Mutex::new(None)),
            completions: Mutex::new(Vec::new()),
            _cache: DiskCache::new(fd, true),
            _file: file,
            indicator: Indicator::new(),
            gate: Arc::new(QuiesceGate::new(1)),
        });

        crate::quiesce::testing::assert_query_non_blocking(&device);
        assert!(!device.is_quiesced());
    }

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

    /// Helper: create a VirtioBlock backed by a temporary file.
    fn make_test_block(size_bytes: u64) -> VirtioBlock {
        use std::io::Write;
        let mut tmpfile = tempfile::tempfile().expect("create tempfile");
        tmpfile.set_len(size_bytes).expect("set file length");
        // Ensure the file has some content so metadata().len() works
        tmpfile.write_all(&[0u8; 1]).expect("write to tempfile");
        tmpfile.flush().expect("flush tempfile");
        let physmap = Arc::new(PhysMap::new());
        let opts = VirtioBlockOpts::default();
        VirtioBlock::new(tmpfile, &opts, physmap, 1).expect("create VirtioBlock")
    }

    #[test]
    fn device_id_returns_valid_20_bytes() {
        let blk = make_test_block(512 * 1024);
        let id = blk.device_id();
        assert_eq!(id.len(), VIRTIO_BLK_ID_BYTES);
        assert_eq!(id.len(), 20);
        // Should start with "vmm-virtio-blk"
        let s = std::str::from_utf8(&id[..14]).unwrap();
        assert_eq!(s, "vmm-virtio-blk");
        // Remaining bytes should be zero-padded
        for &b in &id[14..] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn config_size_returns_ext() {
        let blk = make_test_block(512 * 1024);
        assert_eq!(blk.config_size(), BLK_CONFIG_SIZE_EXT);
        assert_eq!(blk.config_size(), 60);
    }

    #[test]
    fn device_features_includes_expected_bits() {
        let blk = make_test_block(512 * 1024);
        let f = blk.device_features();
        // Must have FLUSH, SIZE_MAX, SEG_MAX
        assert_ne!(f & bits::VIRTIO_BLK_F_FLUSH, 0, "FLUSH not set");
        assert_ne!(f & bits::VIRTIO_BLK_F_SIZE_MAX, 0, "SIZE_MAX not set");
        assert_ne!(f & bits::VIRTIO_BLK_F_SEG_MAX, 0, "SEG_MAX not set");
    }

    #[test]
    fn device_features_includes_discard_write_zeroes() {
        let blk = make_test_block(512 * 1024);
        let f = blk.device_features();
        // Non-readonly device should have DISCARD and WRITE_ZEROES
        assert_ne!(f & bits::VIRTIO_BLK_F_DISCARD, 0, "DISCARD not set");
        assert_ne!(f & bits::VIRTIO_BLK_F_WRITE_ZEROES, 0, "WRITE_ZEROES not set");
    }

    #[test]
    fn device_features_includes_event_idx_and_indirect() {
        let blk = make_test_block(512 * 1024);
        let f = blk.device_features();
        // EVENT_IDX and INDIRECT_DESC are included in device features.
        // Legacy transport strips them in pci.rs; modern advertises them.
        assert_ne!(
            f & bits::VIRTIO_F_RING_EVENT_IDX, 0,
            "EVENT_IDX should be in device features"
        );
        assert_ne!(
            f & bits::VIRTIO_F_RING_INDIRECT_DESC, 0,
            "INDIRECT_DESC should be in device features"
        );
    }

    #[test]
    fn device_features_excludes_ro_for_writable() {
        let blk = make_test_block(512 * 1024);
        let f = blk.device_features();
        assert_eq!(f & bits::VIRTIO_BLK_F_RO, 0, "RO should not be set for writable device");
    }

    #[test]
    fn device_features_includes_ro_for_readonly() {
        use std::io::Write;
        let mut tmpfile = tempfile::tempfile().expect("create tempfile");
        tmpfile.set_len(512 * 1024).expect("set file length");
        tmpfile.write_all(&[0u8; 1]).expect("write");
        tmpfile.flush().expect("flush");
        let physmap = Arc::new(PhysMap::new());
        let opts = VirtioBlockOpts { read_only: true, ..Default::default() };
        let blk = VirtioBlock::new(tmpfile, &opts, physmap, 1).unwrap();
        let f = blk.device_features();
        assert_ne!(f & bits::VIRTIO_BLK_F_RO, 0, "RO should be set for readonly device");
        // Readonly should NOT have DISCARD or WRITE_ZEROES
        assert_eq!(f & bits::VIRTIO_BLK_F_DISCARD, 0);
        assert_eq!(f & bits::VIRTIO_BLK_F_WRITE_ZEROES, 0);
    }

    #[test]
    fn cfg_read_capacity() {
        // 512 * 1024 bytes / 512 sector_size = 1024 sectors
        let blk = make_test_block(512 * 1024);
        // Capacity is at config offset 0x00, 8 bytes (little-endian u64)
        let lo = blk.cfg_read(0x00, 4);
        let hi = blk.cfg_read(0x04, 4);
        let capacity = u64::from(lo) | (u64::from(hi) << 32);
        assert_eq!(capacity, 1024);
    }

    #[test]
    fn cfg_read_size_max() {
        let blk = make_test_block(512 * 1024);
        // size_max at offset 0x08
        let val = blk.cfg_read(0x08, 4);
        assert_eq!(val, SIZE_MAX);
        assert_eq!(val, 128 * 1024);
    }

    #[test]
    fn cfg_read_seg_max() {
        let blk = make_test_block(512 * 1024);
        // seg_max at offset 0x0C
        let val = blk.cfg_read(0x0C, 4);
        assert_eq!(val, SEG_MAX);
        assert_eq!(val, 126);
    }

    #[test]
    fn cfg_read_blk_size() {
        let blk = make_test_block(512 * 1024);
        // blk_size at offset 0x14
        let val = blk.cfg_read(0x14, 4);
        assert_eq!(val, 512);
    }

    #[test]
    fn cfg_read_out_of_range() {
        let blk = make_test_block(512 * 1024);
        // Reading past config space should return 0
        let val = blk.cfg_read(0xFF, 4);
        assert_eq!(val, 0);
    }

    #[test]
    fn num_queues_single() {
        let blk = make_test_block(512 * 1024);
        assert_eq!(blk.num_queues(), 1);
    }

    #[test]
    fn device_features_mq_only_with_multiple_queues() {
        use std::io::Write;
        // Single queue: MQ should not be set
        let blk_single = make_test_block(512 * 1024);
        assert_eq!(blk_single.device_features() & bits::VIRTIO_BLK_F_MQ, 0);

        // Multiple queues: MQ should be set
        let mut tmpfile = tempfile::tempfile().expect("create tempfile");
        tmpfile.set_len(512 * 1024).expect("set len");
        tmpfile.write_all(&[0u8; 1]).expect("write");
        tmpfile.flush().expect("flush");
        let physmap = Arc::new(PhysMap::new());
        let opts = VirtioBlockOpts::default();
        let blk_mq = VirtioBlock::new(tmpfile, &opts, physmap, 2).unwrap();
        assert_ne!(blk_mq.device_features() & bits::VIRTIO_BLK_F_MQ, 0);
    }
}
