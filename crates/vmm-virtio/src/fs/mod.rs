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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmm_core::mem::PhysMap;

use self::passthrough::Passthrough;
use self::server::FuseServer;
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;
use crate::access::GuestAccess;
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

#[cfg(test)]
type ParkSlot = Mutex<Option<Arc<dyn Fn() + Send + Sync>>>;

/// Park points a test uses to pin a reset against the worker.
#[cfg(test)]
#[derive(Default)]
struct FsParks {
    /// Entered where the backing store runs, with no guest access held.
    in_backend: ParkSlot,
}

#[cfg(test)]
fn run_park(slot: &ParkSlot) {
    let hook = slot.lock().expect("park lock poisoned").take();
    if let Some(hook) = hook {
        hook();
    }
}

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
    /// See [`FsWorkerCtx::access`].
    access: Arc<GuestAccess>,
    /// Requests the guest is still owed. See [`FsWorkerCtx::inflight`].
    inflight: Arc<AtomicUsize>,
    /// See [`FsWorkerCtx::session_retired`].
    session_retired: Arc<AtomicBool>,
    #[cfg(test)]
    parks: Arc<FsParks>,
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

        let ctx = FsWorkerCtx {
            server: Arc::clone(&server),
            physmap: Arc::clone(&physmap),
            gate: Arc::new(QuiesceGate::new(1)),
            shutdown: Arc::new(AtomicBool::new(false)),
            access: Arc::new(GuestAccess::new()),
            inflight: Arc::new(AtomicUsize::new(0)),
            session_retired: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            parks: Arc::new(FsParks::default()),
        };
        let (tx, rx) = mpsc::channel::<FsRequest>();

        let worker = std::thread::Builder::new()
            .name("virtio-fs".to_string())
            .spawn({
                let ctx = ctx.clone();
                move || fs_worker(rx, ctx)
            })?;

        Ok(Self {
            tag,
            server,
            physmap,
            negotiated_features: Mutex::new(0),
            completions: Mutex::new(vec![None; FS_NUM_QUEUES]),
            work_tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            shutdown: ctx.shutdown,
            gate: ctx.gate,
            access: ctx.access,
            inflight: ctx.inflight,
            session_retired: ctx.session_retired,
            #[cfg(test)]
            parks: ctx.parks,
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

/// State the FUSE worker shares with the device.
#[derive(Clone)]
struct FsWorkerCtx {
    server: Arc<FuseServer>,
    physmap: Arc<PhysMap>,
    gate: Arc<QuiesceGate>,
    shutdown: Arc<AtomicBool>,
    /// Permission to touch the guest's ring and chains, and the epoch
    /// every request is tagged with; see [`FsRequest::epoch`].
    ///
    /// The worker takes a session around each copy between host memory
    /// and a chain, and around the used entry that follows it.
    /// [`VirtioDevice::reset`] closes admission and drains those
    /// sections, which is how it takes that permission back.
    ///
    /// No section may wait on the backing store, park on the quiesce
    /// gate, or raise an interrupt. A vCPU waits for the drain, so
    /// each section must end on its own.
    access: Arc<GuestAccess>,
    /// Requests the guest is still owed.
    ///
    /// The worker drops its count after it publishes the used entry.
    /// `reset` does not read this: the guard it takes is short whatever
    /// this says.
    inflight: Arc<AtomicUsize>,
    /// Set by a reset, cleared by the worker once it has closed the
    /// retired session's fd tables.
    ///
    /// A flag, not a comparison against `epoch`: the worker would have
    /// to sample the epoch to have something to compare, and a reset
    /// that lands before that sample would then look like the session
    /// the worker started in.
    session_retired: Arc<AtomicBool>,
    #[cfg(test)]
    parks: Arc<FsParks>,
}

/// Drop every request still queued and take them off the count.
///
/// A halt abandons the request the worker holds and everything behind
/// it. `inflight` counts what the guest is still owed, so a request
/// nobody will answer must come off it.
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

fn fs_worker(rx: mpsc::Receiver<FsRequest>, ctx: FsWorkerCtx) {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        let req = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        ctx.gate.park_if_paused();
        if ctx.shutdown.load(Ordering::Acquire) {
            let held = usize::from(req.is_some());
            drop(req);
            abandon_queued(&rx, &ctx.inflight, held);
            return;
        }
        // Before the request is served, so a request from the new
        // session never sees the old session's nodeids and handles.
        reap_retired_session(&ctx);
        let Some(req) = req else { continue };

        serve_request(&ctx, &req);
        drop(req);
        ctx.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Close the fd tables of a driver session that a reset ended.
///
/// The tables are backing-store state, so the reset only marks them
/// and the worker closes them here. Until `halt` joins this thread, it
/// is the only one that closes a guest-held fd, so no syscall can be
/// running on a descriptor this closes. A number closed under such a
/// syscall would go to the next thread in this process that opens
/// anything.
fn reap_retired_session(ctx: &FsWorkerCtx) {
    if ctx.session_retired.swap(false, Ordering::AcqRel) {
        ctx.server.passthrough().clear();
    }
}

/// Run one FUSE request and publish its reply.
///
/// Guest memory is touched twice, each time inside a session for the
/// request's epoch: the request copied out, then the reply copied in
/// together with its used entry. Both copies are bounded, by
/// `MAX_MSG_SIZE` and by the chain's writable window, so a reset waits
/// on neither for long.
///
/// `FuseServer::run` sits between them with no session at all. It
/// reads and writes host memory only, so a host filesystem that never
/// answers stops this device's I/O and holds nothing else.
///
/// Nothing here may park on the quiesce gate. A worker that parked
/// inside a session would wedge the reset of a paused device.
fn serve_request(ctx: &FsWorkerCtx, req: &FsRequest) {
    let call = {
        // A reset already invalidated this chain and its completion.
        // The check comes before the lock: a refused request must not
        // be able to queue on the lock a resetting vCPU waits for.
        let Some(_session) = ctx.access.enter(req.epoch) else {
            return;
        };
        ctx.server.read_request(&req.bufs, &ctx.physmap)
    };

    #[cfg(test)]
    run_park(&ctx.parks.in_backend);
    let reply = ctx.server.run(&call);

    let raise = {
        // A reset landed while the backing store ran. The reply is
        // host memory, so dropping it here writes nothing into the
        // guest.
        let Some(_session) = ctx.access.enter(req.epoch) else {
            return;
        };
        let len = ctx.server.write_reply(&req.bufs, &ctx.physmap, reply);
        req.completion.publish_batch(&[(req.head, len)])
    };
    // Delivery is an ioctl and can block, so it leaves the session
    // behind. A reset drains the sections above, and an interrupt
    // raised inside one puts the resetting vCPU behind it.
    if raise {
        ctx.access.deliver(req.epoch, || req.completion.signal());
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

    /// Collect the guest's chains and hand them to the worker.
    ///
    /// The ring walk runs under one guest-access session, so a reset
    /// takes the ring back from this the same way it takes it back
    /// from the worker. The session also names the epoch every request
    /// is tagged with, so a request cannot carry a session that ended
    /// while it was being built.
    ///
    /// Returns whether the heads answered inline need an interrupt.
    /// The transport raises it after it drops its register lock.
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

        let Some(session) = self.access.enter_current() else {
            // A reset owns the ring. Its queue state goes with it.
            return false;
        };
        let epoch = session.epoch();

        // A guest can keep advancing avail_idx during the EVENT_IDX
        // re-check. Bound the work retained by one notification to the
        // queue size, so the counter lives outside the re-check loop.
        let max_requests = usize::from(queue.size());
        // Heads the worker will never answer, published together at
        // the end so the used ring is written once.
        let mut finished: Vec<(u16, u32)> = Vec::new();
        let mut popped = 0usize;

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
                    finished.push((head, 0));
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
                // The send stays in the session, unlike virtio-blk's
                // dispatch: a head this refuses has to reach the used
                // ring, and the queue caps how many sends there are.
                if tx.send(req).is_err() {
                    self.inflight.fetch_sub(1, Ordering::AcqRel);
                    finished.push((head, 0));
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

        let raise = completion.publish_batch(&finished);
        drop(session);

        // The heads this answered itself go back to the transport for
        // the interrupt, the way virtio-blk does it. Raising here runs
        // under the transport's register lock, and a reset on another
        // vCPU has to take that lock before it can even close
        // admission, so an injection here is one a guest can put in
        // front of a reset. The replies the worker publishes raise
        // their own interrupt, off the vCPU.
        raise
    }

    /// Reset the device, and return once nothing can write into the
    /// ring the driver is about to free.
    ///
    /// This runs on the vCPU that wrote DEVICE_STATUS, so it has to be
    /// finished when it returns and it has to be short. The illumos
    /// legacy driver gives no grace: `virtio_legacy_device_reset_locked`
    /// is one register write with no poll, and `virtio_shutdown`
    /// reclaims the DMA buffers straight after it.
    ///
    /// It is both. Admission closes and the epoch ends, which drops
    /// every request the worker has not started. The drain then covers
    /// only the two bounded copies a request makes into and out of its
    /// chain, so the wait ends on its own and needs no deadline. The
    /// backing store is not waited for: a request inside it finishes
    /// into host memory, fails the epoch check, and writes nothing.
    fn reset(&self) {
        // Admission closes before the drain. A section refused before
        // it takes the lock never queues on it, and `std::sync::RwLock`
        // gives the writer no priority: a guest picks how much stale
        // work is waiting, and it must not be able to starve this.
        self.access.close();
        self.access.drain();

        *self.negotiated_features.lock().expect("features lock") = 0;
        for slot in self.completions.lock().expect("completion lock").iter_mut()
        {
            *slot = None;
        }
        // The guest's fd tables are backing-store state, so this only
        // marks them and the worker closes them. Closing them here
        // would pull a descriptor out from under a syscall the worker
        // is inside. The mark goes up before admission reopens, so no
        // request of the next session is served against them.
        self.session_retired.store(true, Ordering::Release);
        self.access.reopen();
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
