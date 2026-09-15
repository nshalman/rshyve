// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO block device.
//!
//! One invariant holds the design together: a backing-store operation
//! owns only host memory, and every guest-memory access needs
//! permission for the request epoch, held until that access finishes.
//! So a reset closes admission, ends the epoch, waits for the short
//! guest-access sections, and returns. It never waits for the disk.
//!
//! Requests that reach the disk run on worker threads. Only the ones
//! that touch guest memory alone, such as GET_ID, run inline on the
//! vCPU.
//!
//! All completions go through `VirtioCompletion`, one Mutex-protected
//! used ring writer. Separate write indexes on the inline and async
//! paths corrupt the used index.

use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use vmm_devices::blkdev::DiskCache;
use vmm_devices::lifecycle::Indicator;
use vmm_devices::{FlushError, FlushIntent, Lifecycle, QuiesceGate};

use vmm_core::mem::PhysMap;

#[usdt::provider(provider = "vmm")]
mod probes {
    fn vioblk_notify(queue_idx: u16, num_requests: u32) {}
    fn vioblk_inline(head: u16, rtype: u32, sector: u64) {}
    fn vioblk_dispatch(head: u16, rtype: u32, sector: u64) {}
    fn vioblk_complete(head: u16, status: u8) {}
    fn vioblk_reset_drain(epoch: u64) {}
}

use super::bits;
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::{queue_drain_should_stop, VirtioDevice};

mod access;
mod discard;
mod io;
#[cfg(test)]
mod tests;
mod worker;

use crate::access::{GuestAccess, Session};
use crate::pci::intr::{next_intr_session, BackendIntr};
use access::BackendBarrier;
use io::{parse_header, write_status};
use worker::{blk_io_worker, needs_backend, BlkIoRequest, WorkerCtx};

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
    /// `min(num_vcpus, 4)`). Capped at 8: a zvol has one SPA sync
    /// pipeline, so more queues add worker threads and no throughput.
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

/// Backing-store properties a request is checked against. Copied into
/// each I/O worker so a request can be bounded without the device handle.
#[derive(Debug, Clone, Copy)]
struct DiskLimits {
    capacity: u64,
    read_only: bool,
    nodelete: bool,
}

/// VirtIO block device.
pub struct VirtioBlock {
    capacity: u64,
    sector_size: u32,
    read_only: bool,
    /// Suppress DISCARD, matching bhyve's `nodelete` block-device option.
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
    /// The transport interrupt path, set after PCI device creation.
    pub interrupt: Arc<Mutex<Option<Arc<BackendIntr>>>>,
    /// Per-queue completion handlers, created on first notification.
    completions: Mutex<Vec<Option<Arc<VirtioCompletion>>>>,
    /// Zvol write-cache control.
    _cache: DiskCache,
    /// The backing store. Shared with every worker, so the descriptor
    /// outlives a device that is dropped while a syscall is running.
    /// A worker holding a bare fd number would resume against whatever
    /// the host reopened on it.
    file: Arc<File>,
    /// Lifecycle state indicator.
    indicator: Indicator,
    gate: Arc<QuiesceGate>,
    /// Shared with every I/O worker.
    ctx: WorkerCtx,
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
        let file = Arc::new(file);

        // Enable zvol writeback cache (DKIOCSETWCE=1). Must happen before
        // any I/O so the first guest writes already hit the fast path.
        let cache = DiskCache::new(Arc::clone(&file), read_only);

        // Workers per queue: NUM_WORKERS for single queue, 2 per queue for MQ.
        let limits = DiskLimits {
            capacity,
            read_only,
            nodelete: opts.nodelete,
        };
        let workers_per_queue = if num_queues <= 1 { NUM_WORKERS } else { 2 };
        let total_workers = num_queues as usize * workers_per_queue;
        let gate = Arc::new(QuiesceGate::new(total_workers));
        let ctx = WorkerCtx {
            physmap: Arc::clone(&physmap),
            gate: Arc::clone(&gate),
            access: Arc::new(GuestAccess::new()),
            backend: Arc::new(BackendBarrier::new()),
            #[cfg(test)]
            parks: Arc::new(worker::WorkerParks::default()),
        };
        let mut io_txs = Vec::with_capacity(total_workers);
        let mut next_worker = Vec::with_capacity(num_queues as usize);

        for q in 0..num_queues {
            for i in 0..workers_per_queue {
                let (tx, rx) = mpsc::channel::<BlkIoRequest>();
                let ctx = ctx.clone();
                let file = Arc::clone(&file);

                thread::Builder::new()
                    .name(format!("virtio-blk-q{q}-io-{i}"))
                    .spawn(move || {
                        blk_io_worker(rx, file, limits, ctx);
                    })
                    .expect("failed to spawn virtio-blk I/O worker");

                io_txs.push(tx);
            }
            next_worker.push(AtomicUsize::new(0));
        }

        let completions = (0..num_queues as usize).map(|_| None).collect();

        Ok(Self {
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
            file,
            indicator: Indicator::new(),
            gate,
            ctx,
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
    ///
    /// The caller's permission covers two things. The only guest
    /// memory read here is the used index, and the session stamped on
    /// the handler has to be the one the ring belongs to. Permission
    /// is refused for the whole of a reset, so neither can reach the
    /// slot the next driver finds.
    fn get_completion(
        &self,
        access: &Session<'_>,
        queue_idx: u16,
        queue: &VirtQueue,
    ) -> Arc<VirtioCompletion> {
        let mut slots = self.completions.lock().expect("completion lock");
        let idx = queue_idx as usize;
        if idx < slots.len() {
            if let Some(ref c) = slots[idx] {
                return Arc::clone(c);
            }
        }

        let intr = Arc::clone(&self.interrupt);
        // The handler carries the ring it belongs to. MSI-X gives each
        // virtqueue its own vector, and the guest driver only looks at
        // the ring that vector names, so a completion raised on any
        // other index is never collected.
        let interrupt_fn = move |session| {
            // The slot is unlocked for the raise. Delivery is a call
            // into the kernel, and the reset reads this slot, so a
            // raise that held the lock would put that call in front
            // of a vCPU.
            let path = intr.lock().ok().and_then(|slot| slot.clone());
            if let Some(path) = path {
                path.raise(session, queue_idx);
            }
        };

        let c = VirtioCompletion::new(
            queue,
            Arc::clone(&self.physmap),
            access.intr(),
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

    /// Check one chain and return its request header.
    ///
    /// On refusal the status byte is written and the used length the
    /// guest is owed is returned. A refused head still goes back on the
    /// used ring: `pop_avail` has already taken it out of the available
    /// ring, and virtio-blk has no abort path to recover it.
    fn check_request(
        &self,
        chain: &[ChainBuf],
        physmap: &PhysMap,
    ) -> Result<VirtioBlkReqHdr, u32> {
        let refuse = |chain: &[ChainBuf]| {
            write_status(physmap, chain, bits::VIRTIO_BLK_S_IOERR)
        };
        if chain.len() < 2 {
            return Err(refuse(chain));
        }
        let Some(header) = parse_header(chain, physmap) else {
            return Err(refuse(chain));
        };
        // Segment count and segment size are the guest's to pick, so
        // the advertised limits hold only where they are checked.
        let Some(data_bytes) = io::span(chain) else {
            return Err(refuse(chain));
        };

        if header.rtype == bits::VIRTIO_BLK_T_IN
            || header.rtype == bits::VIRTIO_BLK_T_OUT
        {
            // A request with no data buffer would pass the sector range
            // check whatever sector it named, because 0 + 0 is never
            // past the capacity.
            if data_bytes == 0 {
                return Err(refuse(chain));
            }
            let sectors_needed =
                (data_bytes as u64).div_ceil(bits::SECTOR_SIZE);
            if header
                .sector
                .checked_add(sectors_needed)
                .is_none_or(|end| end > self.capacity)
            {
                return Err(refuse(chain));
            }
        }
        Ok(header)
    }

    /// Run a request that touches guest memory and nothing else.
    ///
    /// Returns (status, used_ring_len).
    fn local_request(
        &self,
        header: &VirtioBlkReqHdr,
        chain: &[ChainBuf],
        physmap: &PhysMap,
    ) -> (u8, u32) {
        match header.rtype {
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
            _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
        }
    }

    /// Hand a request to one of the queue's workers.
    fn dispatch(
        &self,
        queue_idx: u16,
        epoch: u64,
        completion: &Arc<VirtioCompletion>,
        head: u16,
        header: &VirtioBlkReqHdr,
        chain: Vec<ChainBuf>,
    ) {
        probes::vioblk_dispatch!(|| (head, header.rtype, header.sector));
        let base = queue_idx as usize * self.workers_per_queue;
        let idx = self.next_worker[queue_idx as usize]
            .fetch_add(1, Ordering::Relaxed)
            % self.workers_per_queue;
        // A closed channel means the worker is gone during shutdown.
        // The request carries an epoch, so nothing else has to undo.
        let _ = self.io_txs[base + idx].send(BlkIoRequest {
            rtype: header.rtype,
            sector: header.sector,
            chain,
            head,
            completion: Arc::clone(completion),
            epoch,
        });
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_features(&self) -> u64 {
        let mut f = bits::VIRTIO_BLK_F_FLUSH
            | bits::VIRTIO_BLK_F_SIZE_MAX
            | bits::VIRTIO_BLK_F_SEG_MAX;
        if self.read_only {
            f |= bits::VIRTIO_BLK_F_RO;
        }
        if self.sector_size != 512 {
            f |= bits::VIRTIO_BLK_F_BLK_SIZE;
        }
        if !self.read_only {
            f |= bits::VIRTIO_BLK_F_WRITE_ZEROES;
            if !self.nodelete {
                f |= bits::VIRTIO_BLK_F_DISCARD;
            }
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
        // num_queues at offset 0x22 (2 bytes), VirtIO block MQ config
        config[0x22..0x24].copy_from_slice(&self.num_queues.to_le_bytes());
        // DISCARD and WRITE_ZEROES limits (VirtIO 1.3 sec 5.2.4). Linux
        // reads a zero here as "no limit" (drivers/block/virtio_blk.c),
        // so every field a negotiated feature implies must be non-zero.
        if !self.read_only {
            if !self.nodelete {
                config[0x24..0x28].copy_from_slice(
                    &discard::MAX_DISCARD_SECTORS.to_le_bytes(),
                );
                config[0x28..0x2C]
                    .copy_from_slice(&discard::MAX_SEG.to_le_bytes());
                // Alignment is counted in 512-byte sectors.
                let align = u64::from(self.sector_size)
                    .div_ceil(bits::SECTOR_SIZE)
                    .max(1) as u32;
                config[0x2C..0x30].copy_from_slice(&align.to_le_bytes());
            }
            config[0x30..0x34].copy_from_slice(
                &discard::MAX_WRITE_ZEROES_SECTORS.to_le_bytes(),
            );
            config[0x34..0x38].copy_from_slice(&discard::MAX_SEG.to_le_bytes());
            // We write zeros instead of deallocating, so never unmap.
            config[0x38] = 0;
        }
        let off = offset as usize;
        if off >= config.len() {
            return 0;
        }
        let mut result = 0u32;
        for i in 0..len.min(4) as usize {
            if off + i < config.len() {
                result |= (config[off + i] as u32) << (i * 8);
            }
        }
        result
    }

    fn cfg_write(&self, _: u16, _: u32, _: u8) {}

    /// Override notify_queue: pop avail, run or dispatch, complete.
    ///
    /// ALL used ring writes go through VirtioCompletion, never through
    /// VirtQueue::push_used(). Two paths tracking the used index
    /// independently make it diverge.
    ///
    /// The whole body runs under one guest-access session and makes no
    /// backing-store call, so a reset waiting for it waits for ring
    /// walks and short memory copies, bounded by the queue size.
    ///
    /// The return value asks the transport to raise the interrupt,
    /// rather than raising it here. This runs under the transport's
    /// register lock, and a reset on another vCPU has to take that
    /// lock before it can start, so an interrupt injected here would
    /// put a call into the kernel on the reset's path. The transport
    /// drops the lock first. Workers raise their own completions:
    /// they hold no transport lock.
    ///
    /// EVENT_IDX re-check loop (VirtIO 1.3 sec 2.7.10):
    /// After draining the ring and writing `used_event`, re-read
    /// `avail_idx`. If the guest added entries during the race
    /// window between our drain and the `used_event` write, the
    /// guest may have decided not to kick, so the device must catch
    /// those entries itself.
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

        let Some(session) = self.ctx.access.enter_current() else {
            // A reset owns the ring. Its queue state goes with it.
            return false;
        };
        let completion = self.get_completion(&session, queue_idx, queue);
        let epoch = session.epoch();

        // A guest can keep advancing avail_idx during the EVENT_IDX re-check.
        // Bound the work retained by one notification to the queue size.
        let max_requests = usize::from(queue.size());
        let mut requests: Vec<(u16, VirtioBlkReqHdr, Vec<ChainBuf>)> =
            Vec::with_capacity(max_requests);
        // Results already finished, published together at the end.
        let mut finished: Vec<(u16, u32)> = Vec::new();
        let mut popped = 0usize;

        loop {
            let before = queue.last_avail_idx();
            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;

                let chain = match queue.collect_chain(physmap, head) {
                    Some(c) => c,
                    None => {
                        // `pop_avail` already took this head out of the
                        // available ring, so it has to go back on the
                        // used ring. Dropping it burns the descriptor
                        // for the life of the device: the block layer
                        // has no abort path for virtio-blk.
                        finished.push((head, 0));
                        continue;
                    }
                };

                let header = match self.check_request(&chain, physmap) {
                    Ok(header) => header,
                    Err(len) => {
                        finished.push((head, len));
                        continue;
                    }
                };

                if needs_backend(header.rtype) {
                    requests.push((head, header, chain));
                    continue;
                }

                probes::vioblk_inline!(|| (head, header.rtype, header.sector));
                let (status, written) =
                    self.local_request(&header, &chain, physmap);
                write_status(physmap, &chain, status);
                probes::vioblk_complete!(|| (head, status));
                finished.push((head, written));
            }

            // A cap exit leaves entries pending, so publishing avail_event
            // would suppress a subsequent single-entry kick.
            let hit_request_cap = popped >= max_requests;
            if !hit_request_cap {
                queue.update_used_event(physmap);
            }
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
            // Guest added entries during race window; drain again.
        }

        probes::vioblk_notify!(|| (
            queue_idx,
            (finished.len() + requests.len()) as u32
        ));

        let raise = completion.publish_batch(&finished);
        drop(session);

        for (head, header, chain) in requests {
            self.dispatch(queue_idx, epoch, &completion, head, &header, chain);
        }

        raise
    }

    fn process_queue(
        &self,
        _: u16,
        _: &mut VirtQueue,
        _: u16,
        _: &PhysMap,
    ) -> u32 {
        0 // not called: notify_queue is overridden
    }

    /// Reset the device, and do not return until no worker can still
    /// write into the guest memory this ring used.
    ///
    /// The illumos driver treats one register write as the whole reset:
    /// `virtio_legacy_device_reset_locked` does not poll, and
    /// `virtio_main.c` states the contract as "when we reset the device,
    /// it should immediately stop using any DMA memory we have
    /// previously passed to it". `virtio_shutdown` then reclaims those
    /// buffers. So this is synchronous.
    ///
    /// It is safe to run synchronously because it does not wait for the
    /// disk, and it does not wait for interrupt delivery either.
    /// Admission closes, the epoch ends, and the wait covers the
    /// guest-memory sections alone. Each of those is a bounded copy or
    /// ring walk, so the wait needs no deadline and has none.
    ///
    /// Ending the epoch stops any further raise for the old session.
    /// A raise already past that check is abandoned, not waited for:
    /// injection is a call into the kernel, and this runs on a vCPU.
    fn reset(&self) {
        // Close admission before the drain. A section refused before it
        // takes the lock never queues on it, and `std::sync::RwLock`
        // gives the writer no priority: a guest picks how much stale
        // work is waiting, and it must not be able to starve this.
        let epoch = self.ctx.access.close();
        probes::vioblk_reset_drain!(|| epoch);
        // No backend work is admitted for the old session from here.
        // Operations already inside a syscall are counted until they
        // end, and the next session's I/O waits behind them.
        self.ctx.backend.retire(epoch);
        self.ctx.access.drain();

        *self.negotiated_features.lock().expect("lock") = 0;
        for slot in self.completions.lock().expect("lock").iter_mut() {
            *slot = None;
        }
        self.ctx.access.reopen(next_intr_session(
            &self.interrupt,
            self.ctx.access.intr_session(),
        ));
    }
}

impl Lifecycle for VirtioBlock {
    fn type_name(&self) -> &'static str {
        "virtio-blk"
    }

    fn lifecycle_state(
        &self,
    ) -> Option<vmm_devices::lifecycle::IndicatedState> {
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
        vmm_devices::quiesce::flush_file(
            &self.gate,
            &self.file,
            intent,
            "virtio-blk",
        )
    }
}
