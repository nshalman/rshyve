// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! virtio-fs: FUSE over a virtqueue, with a host passthrough backend.
//!
//! No DAX and no shared-memory window: every reply travels through the
//! request queue, so the guest sees a plain FUSE transport.

pub mod fuse;
pub mod passthrough;
pub mod server;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmm_core::mem::PhysMap;

use self::passthrough::Passthrough;
use self::server::FuseServer;
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;
use vmm_devices::lifecycle::{IndicatedState, Indicator};
use vmm_devices::{FlushError, FlushIntent, Lifecycle, QuiesceGate};

/// Queue 0 is hiprio, queues 1..=N are request queues (VirtIO 1.3 5.11).
pub const FS_NUM_QUEUES: usize = 2;
/// Hiprio queue. One worker serves both queues, so a FUSE_INTERRUPT or
/// FORGET waits behind whatever request is already in flight.
pub const FS_HIPRIO_QUEUE: u16 = 0;
pub const FS_REQUEST_QUEUE: u16 = 1;
pub const FS_NUM_REQUEST_QUEUES: u32 = 1;
/// Full `struct virtio_fs_config`: tag[36] + num_request_queues +
/// notify_buf_size. Sized to the whole struct because Linux bounds-checks
/// `offset + len` against the advertised device-config length.
pub const FS_CONFIG_SIZE: u16 = 44;
pub const FS_TAG_LEN: usize = 36;
pub const FS_QUEUE_SIZE_DEFAULT: u16 = 128;
pub const FS_QUEUE_SIZE_MIN: u16 = 8;
pub const FS_QUEUE_SIZE_MAX: u16 = 1024;
/// One MSI-X vector per queue, plus one for a config change.
pub const FS_MSIX_VECTORS: u16 = FS_NUM_QUEUES as u16 + 1;
/// Completions forced through while the guest's EVENT_IDX threshold is
/// still settling, so an early reply always wakes the fuse driver.
const FS_FORCE_INTERRUPTS: u32 = 32;
/// Budget for the worker to finish its request during halt.
const FS_HALT_BUDGET: Duration = Duration::from_secs(5);
/// How long [`VirtioDevice::reset`] waits for the worker to drain.
///
/// The guest drives the reset, so the wait must be bounded: an unbounded
/// one is a guest-triggerable vCPU hang. Requests that have not started
/// are dropped by the epoch check, so this budget only ever covers a
/// passthrough syscall already in flight.
const FS_RESET_QUIESCE_BUDGET: Duration = Duration::from_secs(2);

/// Interrupt callback, wired by the binary after `VirtioPciDevice::new`.
///
/// Takes the queue index: virtio-fs has two queues and each raises its own
/// MSI-X vector.
pub type QueueInterruptFn = Arc<dyn Fn(u16) + Send + Sync>;

/// Parsed `-s <slot>,virtio-fs,<config>` options.
#[derive(Debug, Clone)]
pub struct VirtioFsOpts {
    /// Mount tag the guest sees in device config.
    pub tag: String,
    /// Host directory exported to the guest.
    pub path: std::path::PathBuf,
    pub read_only: bool,
    /// Requested per-queue size, normalized by [`clamp_queue_size`].
    pub queue_size: u16,
}

impl Default for VirtioFsOpts {
    fn default() -> Self {
        Self {
            tag: String::new(),
            path: std::path::PathBuf::new(),
            read_only: false,
            queue_size: FS_QUEUE_SIZE_DEFAULT,
        }
    }
}

/// Parse the config half of `-s <slot>,virtio-fs,<config>`.
///
/// Grammar: `<host-path>[,tag=<name>][,ro][,queue-size=<n>]`. The tag
/// defaults to the last path component. Unknown options are a hard error:
/// virtio-fs is new here, so there is no forward compatibility to keep.
pub fn parse_fs_config(config: &str) -> anyhow::Result<VirtioFsOpts> {
    let mut parts = config.split(',');
    let path = parts.next().unwrap_or("");
    if path.is_empty() {
        anyhow::bail!("virtio-fs requires a host directory path");
    }

    let mut opts = VirtioFsOpts {
        path: std::path::PathBuf::from(path),
        ..VirtioFsOpts::default()
    };
    let mut tag: Option<String> = None;

    for opt in parts.filter(|o| !o.is_empty()) {
        if let Some(v) = opt.strip_prefix("tag=") {
            tag = Some(v.to_string());
        } else if opt == "ro" {
            opts.read_only = true;
        } else if let Some(v) = opt.strip_prefix("queue-size=") {
            let n: u32 = v.parse().unwrap_or(0);
            if !(u32::from(FS_QUEUE_SIZE_MIN)..=u32::from(FS_QUEUE_SIZE_MAX))
                .contains(&n)
            {
                anyhow::bail!(
                    "virtio-fs queue-size must be {}..={}, got {}",
                    FS_QUEUE_SIZE_MIN,
                    FS_QUEUE_SIZE_MAX,
                    v
                );
            }
            opts.queue_size = n as u16;
        } else {
            anyhow::bail!("virtio-fs: unknown option '{}'", opt);
        }
    }

    opts.tag = match tag {
        Some(t) => t,
        None => opts
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if opts.tag.len() > FS_TAG_LEN {
        anyhow::bail!(
            "virtio-fs tag '{}' exceeds {} bytes",
            opts.tag,
            FS_TAG_LEN
        );
    }
    if opts.tag.as_bytes().contains(&0) {
        anyhow::bail!("virtio-fs tag must not contain NUL");
    }
    Ok(opts)
}

/// Normalize a requested queue size to something `VirtQueue::new` accepts:
/// clamped to 8..=1024, then rounded up to a power of two.
///
/// `VirtQueue::new` panics on a non-power-of-two size, so the catalog must
/// pass this result to `VirtioPciDevice::new`, not the raw user value.
pub fn clamp_queue_size(requested: u16) -> u16 {
    let n = requested.clamp(FS_QUEUE_SIZE_MIN, FS_QUEUE_SIZE_MAX);
    n.next_power_of_two().min(FS_QUEUE_SIZE_MAX)
}

/// One descriptor chain handed to the FUSE worker.
struct FsRequest {
    head: u16,
    bufs: Vec<ChainBuf>,
    completion: Arc<VirtioCompletion>,
    /// Device epoch this request was queued under. The worker drops the
    /// request when it no longer matches: the chain and the completion
    /// both point at a ring the driver has since torn down.
    epoch: u64,
}

pub struct VirtioFs {
    tag: [u8; FS_TAG_LEN],
    server: Arc<FuseServer>,
    physmap: Arc<PhysMap>,
    negotiated_features: Mutex<u64>,
    /// Per-queue completion handlers, created lazily on first notification.
    completions: Mutex<Vec<Option<Arc<VirtioCompletion>>>>,
    work_tx: Mutex<Option<mpsc::Sender<FsRequest>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
    gate: Arc<QuiesceGate>,
    /// Bumped by every device reset. Tags each queued request so the
    /// worker can tell one that predates the reset from a live one.
    epoch: Arc<AtomicU64>,
    /// Requests queued and not yet finished. Reset only has to wait for
    /// the worker while this is non-zero, which keeps a reset on an idle
    /// export off the worker's 50ms park path.
    inflight: Arc<AtomicUsize>,
    indicator: Indicator,
    log: slog::Logger,
    /// Queue-indexed interrupt callback. Clone this Arc BEFORE moving the
    /// device into `VirtioPciDevice::new`.
    pub interrupt: Arc<Mutex<Option<QueueInterruptFn>>>,
}

impl VirtioFs {
    /// Open the passthrough root, build the FUSE server, and spawn the
    /// worker thread.
    pub fn new(
        opts: &VirtioFsOpts,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        let pt = Passthrough::new(&opts.path, opts.read_only).map_err(|e| {
            anyhow::anyhow!(
                "open passthrough root '{}': {}",
                opts.path.display(),
                std::io::Error::from_raw_os_error(e.to_errno())
            )
        })?;
        let server = Arc::new(FuseServer::new(Arc::new(pt)));

        // The options struct is public, so a caller can reach here past
        // parse_fs_config. Refuse a tag the config field cannot hold
        // rather than panic in copy_from_slice.
        let bytes = opts.tag.as_bytes();
        anyhow::ensure!(
            bytes.len() <= FS_TAG_LEN,
            "virtio-fs tag '{}' exceeds {} bytes",
            opts.tag,
            FS_TAG_LEN
        );
        let mut tag = [0u8; FS_TAG_LEN];
        tag[..bytes.len()].copy_from_slice(bytes);

        let shutdown = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(QuiesceGate::new(1));
        let epoch = Arc::new(AtomicU64::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<FsRequest>();

        let worker = std::thread::Builder::new()
            .name("virtio-fs".to_string())
            .spawn({
                let server = Arc::clone(&server);
                let physmap = Arc::clone(&physmap);
                let gate = Arc::clone(&gate);
                let shutdown = Arc::clone(&shutdown);
                let epoch = Arc::clone(&epoch);
                let inflight = Arc::clone(&inflight);
                move || {
                    fs_worker(
                        rx, server, physmap, gate, shutdown, epoch, inflight,
                    )
                }
            })?;

        Ok(Self {
            tag,
            server,
            physmap,
            negotiated_features: Mutex::new(0),
            completions: Mutex::new(vec![None; FS_NUM_QUEUES]),
            work_tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            shutdown,
            gate,
            epoch,
            inflight,
            indicator: Indicator::new(),
            log,
            interrupt: Arc::new(Mutex::new(None)),
        })
    }

    /// Get or create the completion handler for one queue.
    fn get_completion(
        &self,
        queue_idx: u16,
        queue: &VirtQueue,
    ) -> Arc<VirtioCompletion> {
        let mut slots = self.completions.lock().expect("completion lock");
        let idx = usize::from(queue_idx);
        if let Some(Some(c)) = slots.get(idx) {
            return Arc::clone(c);
        }

        let intr = Arc::clone(&self.interrupt);
        let c = VirtioCompletion::new(queue, Arc::clone(&self.physmap), {
            move || {
                if let Ok(guard) = intr.lock() {
                    if let Some(ref f) = *guard {
                        f(queue_idx);
                    }
                }
            }
        });
        c.set_force_interrupt(FS_FORCE_INTERRUPTS);
        if let Some(slot) = slots.get_mut(idx) {
            *slot = Some(Arc::clone(&c));
        }
        c
    }
}

/// Drop every request still queued and take them off the count.
///
/// A halt abandons the request the worker holds and everything behind
/// it. Leaving those counted makes the next reset wait its whole drain
/// budget for a worker that has already gone.
fn abandon_queued(
    rx: &mpsc::Receiver<FsRequest>,
    inflight: &AtomicUsize,
    held: usize,
) {
    let mut count = held;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    if count != 0 {
        inflight.fetch_sub(count, Ordering::AcqRel);
    }
}

fn fs_worker(
    rx: mpsc::Receiver<FsRequest>,
    server: Arc<FuseServer>,
    physmap: Arc<PhysMap>,
    gate: Arc<QuiesceGate>,
    shutdown: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    inflight: Arc<AtomicUsize>,
) {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        let req = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                gate.park_if_paused();
                if shutdown.load(Ordering::Acquire) {
                    abandon_queued(&rx, &inflight, 0);
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        gate.park_if_paused();
        if shutdown.load(Ordering::Acquire) {
            drop(req);
            abandon_queued(&rx, &inflight, 1);
            return;
        }
        // A reset already invalidated this chain and its completion.
        // Drop the request without touching guest memory.
        if req.epoch == epoch.load(Ordering::Acquire) {
            let len = server.handle(&req.bufs, &physmap);
            // A reset can land while the passthrough syscall runs; the
            // used ring it would publish to is gone.
            if req.epoch == epoch.load(Ordering::Acquire) {
                req.completion.complete(req.head, len);
            }
        }
        drop(req);
        inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl VirtioDevice for VirtioFs {
    fn device_features(&self) -> u64 {
        // VERSION_1 is added by the transport, and the transport strips
        // both of these on the legacy path.
        super::bits::VIRTIO_F_RING_INDIRECT_DESC
            | super::bits::VIRTIO_F_RING_EVENT_IDX
    }

    fn set_features(&self, features: u64) {
        // The modern transport calls this twice, once per 32-bit half, so
        // store the value and never act on it here.
        *self.negotiated_features.lock().expect("features lock") = features;
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let mut out = 0u32;
        for i in 0..usize::from(len).min(4) {
            // The guest picks the offset, so the sum is checked.
            let Some(off) = usize::from(offset).checked_add(i) else {
                break;
            };
            let byte = if off < FS_TAG_LEN {
                self.tag[off]
            } else if off < FS_TAG_LEN + 4 {
                FS_NUM_REQUEST_QUEUES.to_le_bytes()[off - FS_TAG_LEN]
            } else {
                // notify_buf_size, and everything past the config, read 0.
                0
            };
            out |= u32::from(byte) << (i * 8);
        }
        out
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        // virtio_fs_config is read-only.
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        // notify_queue is overridden; completion is asynchronous.
        0
    }

    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(usize::from(queue_idx)) else {
            return false;
        };
        let Some(tx) = self.work_tx.lock().expect("work lock").clone() else {
            return false;
        };
        let completion = self.get_completion(queue_idx, queue);

        // A guest can keep advancing avail_idx during the EVENT_IDX
        // re-check. Bound the work retained by one notification to the
        // queue size, so the counter lives outside the re-check loop.
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        let epoch = self.epoch.load(Ordering::Acquire);

        loop {
            let before = queue.last_avail_idx();

            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                let Some(bufs) = queue.collect_chain(physmap, head) else {
                    // A malformed chain completes with zero bytes so the
                    // ring index does not stall behind it.
                    completion.complete(head, 0);
                    continue;
                };
                let req = FsRequest {
                    head,
                    bufs,
                    completion: Arc::clone(&completion),
                    epoch,
                };
                // Count the request before it can be received, so a reset
                // never reads zero while one is already on its way.
                self.inflight.fetch_add(1, Ordering::AcqRel);
                if tx.send(req).is_err() {
                    self.inflight.fetch_sub(1, Ordering::AcqRel);
                    completion.complete(head, 0);
                }
            }

            let hit_request_cap = popped >= max_requests;
            if !hit_request_cap {
                queue.update_used_event(physmap);
            }
            // The no-progress check is what stops a guest-triggerable vCPU
            // hang in this re-check loop (fixed for virtio-blk in b8b429e).
            if super::queue_drain_should_stop(
                queue,
                before,
                hit_request_cap,
                physmap,
            ) {
                break;
            }
        }

        // The completion handler raises the interrupt when the reply lands.
        false
    }

    /// Reset the device, and do not return until the worker can no longer
    /// write into the guest memory this ring used.
    ///
    /// The driver frees the vring as soon as it reads DEVICE_STATUS back
    /// as 0, so a reply that outlives the reset lands in pages the guest
    /// has recycled. VirtIO 1.3 sec 4.1.4.3.1 makes the driver poll for
    /// that 0, which is what buys the device the time to drain.
    ///
    /// Bumping the epoch first drops every request that has not started,
    /// so the wait only ever covers a syscall already in flight. Missing
    /// the budget still completes the reset: the epoch keeps the straggler
    /// off the used ring, and leaving the device half-reset would strand
    /// the driver in that poll loop instead.
    fn reset(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);

        // The transport clears DRIVER_OK before it calls this, so nothing
        // can add to the count once it reads zero. Skipping the drain then
        // keeps a reset on an idle export off the worker's 50ms park path.
        let mut quiesced = true;
        if self.inflight.load(Ordering::Acquire) != 0 {
            // A scope, not a pause: this drain is guest driven, and a
            // migration pause that lands inside it belongs to the
            // control plane and must outlive it.
            let _drain = self.gate.drain_scope();
            quiesced = self.gate.wait_quiesced(FS_RESET_QUIESCE_BUDGET);
        }

        *self.negotiated_features.lock().expect("features lock") = 0;
        for slot in self.completions.lock().expect("completion lock").iter_mut()
        {
            *slot = None;
        }
        // A guest reboot must not leak the inode and handle fd tables,
        // but only the drain makes closing them safe: the worker holds
        // raw fds from these tables across a FUSE syscall, and a number
        // closed under it is handed to the next thread in this process
        // that opens anything. A drain that missed its budget leaves
        // the tables for `halt` to reclaim.
        if quiesced {
            self.server.passthrough().clear();
        }

        // Guest reachable: a driver can rebind virtio-fs in a loop.
        slog::debug!(self.log, "virtio-fs reset"; "quiesced" => quiesced);
    }
}

impl Lifecycle for VirtioFs {
    fn type_name(&self) -> &'static str {
        "virtio-fs"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
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
        // MUST NOT BLOCK: a plain atomic load. Joining happens in halt.
        self.gate.is_quiesced()
    }

    fn resume(&self) {
        self.gate.resume();
        self.indicator.resume();
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shutdown.store(true, Ordering::Release);
        // Drop the sender so the worker's recv disconnects.
        *self.work_tx.lock().expect("work lock") = None;
        self.gate.resume();

        let mut worker_gone = true;
        if let Some(handle) = self.worker.lock().expect("worker lock").take() {
            let deadline = Instant::now() + FS_HALT_BUDGET;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                worker_gone = false;
                slog::warn!(
                    self.log,
                    "virtio-fs worker did not exit within its budget"
                );
            }
        }

        // Drop is unreachable while the device Arc lives in the binary's
        // device vectors, so release guest-held fds here. A worker that
        // outlived its budget still holds fds from these tables, and
        // closing one under it lets an unrelated thread be handed that
        // number, so the leak is the lesser fault.
        if worker_gone {
            self.server.passthrough().clear();
        }
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        match intent {
            FlushIntent::Durable => self
                .server
                .passthrough()
                .sync_all()
                .map_err(FlushError::Sync),
            // A passthrough export writes through to host files with
            // pwrite; there is no device-level writeback cache to lose.
            FlushIntent::BestEffort => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
