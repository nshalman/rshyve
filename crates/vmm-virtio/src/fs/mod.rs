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
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        gate.park_if_paused();
        if shutdown.load(Ordering::Acquire) {
            // A halt abandons this request, so the count must not keep it.
            drop(req);
            inflight.fetch_sub(1, Ordering::AcqRel);
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
mod tests {
    use super::*;

    use std::time::Instant;

    use crate::queue::VirtqDesc;
    use vmm_devices::Lifecycle;

    const DESC_GPA: u64 = 0x1000;
    const AVAIL_GPA: u64 = 0x1100;
    const USED_GPA: u64 = 0x1200;
    const REQ_GPA: u64 = 0x2000;
    const RESP_GPA: u64 = 0x2800;

    fn test_logger() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn test_fs(physmap: Arc<PhysMap>) -> VirtioFs {
        let opts = VirtioFsOpts {
            tag: "shared".to_string(),
            path: std::env::temp_dir(),
            read_only: true,
            queue_size: FS_QUEUE_SIZE_DEFAULT,
        };
        VirtioFs::new(&opts, physmap, test_logger()).expect("open export")
    }

    /// Build a two-descriptor chain and an avail ring that publishes it
    /// `avail_idx` times, mirroring `block.rs`'s ring fixture.
    fn ring(
        queue_size: u16,
        avail_idx: u16,
    ) -> (Arc<PhysMap>, [VirtQueue; FS_NUM_QUEUES], VirtioFs) {
        let physmap = Arc::new(
            PhysMap::new_anon(DESC_GPA, 0x3000).expect("create queue memory"),
        );
        let mut queue = VirtQueue::new(queue_size);
        queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
        queue.set_event_idx(true);

        physmap
            .lookup(DESC_GPA, 16)
            .expect("mapped request descriptor")
            .write(&VirtqDesc {
                addr: REQ_GPA,
                len: 64,
                flags: crate::bits::VRING_DESC_F_NEXT,
                next: 1,
            })
            .expect("write request descriptor");
        physmap
            .lookup(DESC_GPA + 16, 16)
            .expect("mapped reply descriptor")
            .write(&VirtqDesc {
                addr: RESP_GPA,
                len: 256,
                flags: crate::bits::VRING_DESC_F_WRITE,
                next: 0,
            })
            .expect("write reply descriptor");

        physmap
            .lookup(AVAIL_GPA + 2, 2)
            .expect("mapped avail index")
            .write::<u16>(&avail_idx)
            .expect("write avail index");
        for idx in 0..queue_size {
            physmap
                .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
                .expect("mapped avail entry")
                .write::<u16>(&0)
                .expect("write avail entry");
        }

        let fs = test_fs(Arc::clone(&physmap));
        // Queue 0 is hiprio and stays unconfigured; the tests drive the
        // request queue at the index the transport uses.
        (physmap, [VirtQueue::new(queue_size), queue], fs)
    }

    fn avail_event(physmap: &PhysMap, queue: &VirtQueue) -> u16 {
        let gpa = queue.used_addr() + 4 + u64::from(queue.size()) * 8;
        physmap
            .lookup(gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event")
    }

    #[test]
    fn config_requires_a_path() {
        let err = parse_fs_config("").expect_err("empty config");
        assert_eq!(err.to_string(), "virtio-fs requires a host directory path");
    }

    #[test]
    fn tag_defaults_to_the_last_path_component() {
        let opts = parse_fs_config("/export/shared").expect("parse");
        assert_eq!(opts.tag, "shared");
        assert_eq!(opts.path, std::path::PathBuf::from("/export/shared"));
        assert!(!opts.read_only);
        assert_eq!(opts.queue_size, FS_QUEUE_SIZE_DEFAULT);
    }

    #[test]
    fn options_are_parsed() {
        let opts = parse_fs_config("/export/x,tag=data,ro,queue-size=256")
            .expect("parse");
        assert_eq!(opts.tag, "data");
        assert!(opts.read_only);
        assert_eq!(opts.queue_size, 256);
    }

    #[test]
    fn oversized_tag_is_rejected() {
        let tag = "t".repeat(FS_TAG_LEN + 1);
        let err = parse_fs_config(&format!("/export/x,tag={tag}"))
            .expect_err("long tag");
        assert_eq!(
            err.to_string(),
            format!("virtio-fs tag '{tag}' exceeds {FS_TAG_LEN} bytes")
        );
    }

    #[test]
    fn out_of_range_queue_size_is_rejected() {
        let err =
            parse_fs_config("/export/x,queue-size=4").expect_err("too small");
        assert_eq!(
            err.to_string(),
            "virtio-fs queue-size must be 8..=1024, got 4"
        );
    }

    // virtio-fs is new, so there is no forward compatibility to preserve
    // and an unknown option is a hard error, unlike parse_blk_config.
    #[test]
    fn unknown_option_is_rejected() {
        let err = parse_fs_config("/export/x,dax").expect_err("unknown");
        assert_eq!(err.to_string(), "virtio-fs: unknown option 'dax'");
    }

    // VirtQueue::new panics on a non-power-of-two size, so the catalog must
    // pass clamp_queue_size's result to VirtioPciDevice::new.
    #[test]
    fn queue_size_is_clamped_and_rounded_up() {
        assert_eq!(clamp_queue_size(0), FS_QUEUE_SIZE_MIN);
        assert_eq!(clamp_queue_size(8), 8);
        assert_eq!(clamp_queue_size(100), 128);
        assert_eq!(clamp_queue_size(u16::MAX), FS_QUEUE_SIZE_MAX);
        for n in [0u16, 1, 7, 8, 9, 100, 512, 1023, 1024, 4096, u16::MAX] {
            let q = clamp_queue_size(n);
            assert!(q.is_power_of_two(), "{q} is not a power of two");
            assert!((FS_QUEUE_SIZE_MIN..=FS_QUEUE_SIZE_MAX).contains(&q));
        }
    }

    // Linux's virtio_cread bounds-checks offset+len against the advertised
    // device-config length, so FS_CONFIG_SIZE covers the whole spec struct.
    #[test]
    fn config_size_covers_tag_and_both_trailing_fields() {
        assert_eq!(FS_CONFIG_SIZE as usize, FS_TAG_LEN + 4 + 4);
        assert_eq!(FS_CONFIG_SIZE, 44);
    }

    #[test]
    fn a_tag_the_config_field_cannot_hold_is_refused_by_new() {
        let opts = VirtioFsOpts {
            tag: "t".repeat(FS_TAG_LEN + 1),
            path: std::env::temp_dir(),
            read_only: true,
            queue_size: FS_QUEUE_SIZE_DEFAULT,
        };
        let Err(err) =
            VirtioFs::new(&opts, Arc::new(PhysMap::new()), test_logger())
        else {
            panic!("an oversized tag must not build a device");
        };
        assert!(
            err.to_string()
                .ends_with(&format!("exceeds {FS_TAG_LEN} bytes")),
            "got: {err}"
        );
    }

    #[test]
    fn queue_count_fits_the_transport_limit() {
        // VirtioPciDevice::new asserts num_queues <= MAX_QUEUES (4).
        assert!(FS_NUM_QUEUES <= 4);
        assert_eq!(FS_HIPRIO_QUEUE, 0);
        assert_eq!(FS_REQUEST_QUEUE, 1);
    }

    #[test]
    fn device_config_serves_the_tag_then_num_request_queues() {
        let fs = test_fs(Arc::new(PhysMap::new()));

        let mut tag = Vec::new();
        for off in 0..FS_TAG_LEN as u16 {
            tag.push(fs.cfg_read(off, 1) as u8);
        }
        assert_eq!(&tag[..6], b"shared");
        assert!(tag[6..].iter().all(|b| *b == 0));

        assert_eq!(fs.cfg_read(36, 4), FS_NUM_REQUEST_QUEUES);
        assert_eq!(fs.cfg_read(40, 4), 0, "notify_buf_size");
        assert_eq!(fs.cfg_read(FS_CONFIG_SIZE, 4), 0, "past the config");

        let feat = fs.device_features();
        assert_ne!(feat & crate::bits::VIRTIO_F_RING_EVENT_IDX, 0);
        assert_ne!(feat & crate::bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
    }

    // The used ring belongs to VirtioCompletion, so notify_queue must
    // drain the avail ring and update avail_event without ever calling
    // push_used. shadow_used_idx only moves on push_used, so it is the
    // assertion that stays deterministic against the worker thread.
    #[test]
    fn notify_queue_drains_the_ring_and_never_pushes_used() {
        let (physmap, mut queues, fs) = ring(8, 2);
        let req = usize::from(FS_REQUEST_QUEUE);

        assert!(!fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap));

        assert_eq!(queues[req].last_avail_idx(), 2);
        assert_eq!(avail_event(&physmap, &queues[req]), 2);
        assert_eq!(queues[req].read_used_idx(&physmap), 0);
    }

    // A guest can keep advancing avail_idx during the EVENT_IDX re-check.
    // One notification must retain at most one queue's worth of work.
    #[test]
    fn notify_queue_caps_collection_at_queue_size() {
        const AVAIL_EVENT_SENTINEL: u16 = 0x55aa;

        // Four published entries keep has_new_avail() true after this
        // two-entry queue has processed one queue's worth of work.
        let (physmap, mut queues, fs) = ring(2, 4);
        let req = usize::from(FS_REQUEST_QUEUE);
        let gpa =
            queues[req].used_addr() + 4 + u64::from(queues[req].size()) * 8;
        physmap
            .lookup(gpa, 2)
            .expect("mapped avail event")
            .write::<u16>(&AVAIL_EVENT_SENTINEL)
            .expect("write avail event sentinel");

        fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

        assert_eq!(queues[req].last_avail_idx(), queues[req].size());
        assert!(queues[req].has_new_avail(&physmap));
        assert_eq!(avail_event(&physmap, &queues[req]), AVAIL_EVENT_SENTINEL);
        assert_eq!(queues[req].read_used_idx(&physmap), 0);
    }

    #[test]
    fn reset_clears_the_negotiated_features_and_completions() {
        let (physmap, mut queues, fs) = ring(8, 1);
        fs.set_features(crate::bits::VIRTIO_F_RING_EVENT_IDX);
        fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
        assert!(fs.completions.lock().expect("completions")
            [usize::from(FS_REQUEST_QUEUE)]
        .is_some());

        VirtioDevice::reset(&fs);

        assert_eq!(*fs.negotiated_features.lock().expect("features"), 0);
        assert!(fs
            .completions
            .lock()
            .expect("completions")
            .iter()
            .all(Option::is_none));
    }

    // A guest that writes 0 to DEVICE_STATUS frees the vring right after.
    // A reply the worker already dequeued must not land in the used ring
    // the guest has recycled.
    #[test]
    fn reset_drops_a_request_queued_before_it() {
        let (physmap, mut queues, fs) = ring(8, 1);
        let req = usize::from(FS_REQUEST_QUEUE);

        // Park the worker so the dispatch below is observable: the
        // request sits in the channel instead of racing to completion.
        fs.pause();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs.is_quiesced() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fs.is_quiesced(), "worker did not park within the budget");

        fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
        assert_eq!(
            queues[req].read_used_ring_idx(&physmap),
            0,
            "the parked worker must not have run yet"
        );

        VirtioDevice::reset(&fs);
        fs.resume();

        // A woken worker publishes within microseconds, so this window is
        // orders of magnitude of margin.
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            assert_eq!(
                queues[req].read_used_ring_idx(&physmap),
                0,
                "the worker published into the pre-reset used ring"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // Reset pauses the worker to drain it. It must hand the gate back, or
    // the next driver's first request never completes.
    #[test]
    fn reset_leaves_the_worker_runnable() {
        let (physmap, mut queues, fs) = ring(8, 1);
        let req = usize::from(FS_REQUEST_QUEUE);

        VirtioDevice::reset(&fs);
        assert!(!fs.gate.is_paused(), "reset left the worker parked");

        fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

        let deadline = Instant::now() + Duration::from_secs(5);
        while queues[req].read_used_ring_idx(&physmap) == 0
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            queues[req].read_used_ring_idx(&physmap),
            1,
            "a post-reset request never completed"
        );
    }

    // A guest resets its virtio devices several times while it boots. The
    // worker parks 50ms after it goes idle, so a reset with nothing
    // outstanding must skip the drain rather than pay that each time.
    #[test]
    fn reset_with_an_idle_export_skips_the_drain() {
        let (physmap, mut queues, fs) = ring(8, 1);

        fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs.inflight.load(Ordering::Acquire) != 0
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            fs.inflight.load(Ordering::Acquire),
            0,
            "the queued request never finished"
        );

        let start = Instant::now();
        VirtioDevice::reset(&fs);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(40),
            "reset drained an idle export: {elapsed:?}"
        );
    }

    // Clearing the fd tables is safe only because the drain says the
    // worker has let go of them. A drain that missed its budget says
    // nothing of the sort: the worker still holds a raw fd across a
    // FUSE syscall, and closing it hands that number to the next
    // thread in this process that opens anything.
    #[test]
    fn a_reset_that_missed_its_drain_keeps_the_fd_tables() {
        let dir = tempfile::tempdir().expect("export directory");
        // `bind_restricted` moves the process umask while it binds, and
        // the vsock tests bind in parallel with this one, so the mode
        // this directory is created with cannot be trusted.
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("export directory mode");
        std::fs::write(dir.path().join("f"), b"hello").expect("seed file");
        let opts = VirtioFsOpts {
            tag: "shared".to_string(),
            path: dir.path().to_path_buf(),
            read_only: true,
            queue_size: FS_QUEUE_SIZE_DEFAULT,
        };
        let fs = VirtioFs::new(&opts, Arc::new(PhysMap::new()), test_logger())
            .expect("open export");

        let name = std::ffi::CString::new("f").expect("file name");
        let entry = fs
            .server
            .passthrough()
            .lookup(fuse::FUSE_ROOT_ID, &name)
            .expect("look the file up");

        // End the worker without a halt: it never parks, so the gate
        // cannot quiesce and the drain has to miss its budget.
        fs.shutdown.store(true, Ordering::Release);
        *fs.work_tx.lock().expect("work lock") = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done = fs
                .worker
                .lock()
                .expect("worker lock")
                .as_ref()
                .is_none_or(std::thread::JoinHandle::is_finished);
            if done || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        fs.inflight.fetch_add(1, Ordering::AcqRel);

        VirtioDevice::reset(&fs);

        fs.server
            .passthrough()
            .getattr(entry.nodeid)
            .expect("the reset closed an fd the worker still held");
    }

    // The reset drain is guest driven: any write of 0 to DEVICE_STATUS
    // reaches it, in a loop if the guest likes. A migration or an
    // operator pause that lands while it runs belongs to the control
    // plane, and the reset must not hand the workers back when it ends.
    #[test]
    fn a_reset_drain_does_not_release_a_pause_that_lands_in_it() {
        let fs = test_fs(Arc::new(PhysMap::new()));
        fs.start().expect("start");

        // End the worker without a halt: it never parks, so the drain
        // runs for its whole budget and the pause below lands inside it.
        fs.shutdown.store(true, Ordering::Release);
        *fs.work_tx.lock().expect("work lock") = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done = fs
                .worker
                .lock()
                .expect("worker lock")
                .as_ref()
                .is_none_or(std::thread::JoinHandle::is_finished);
            if done || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        fs.inflight.fetch_add(1, Ordering::AcqRel);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                fs.pause();
            });
            VirtioDevice::reset(&fs);
        });

        assert!(
            fs.gate.is_paused(),
            "the reset released a pause the control plane owns"
        );
    }

    // halt runs only from the paused state, and it must join the worker
    // it unparks rather than leaving the thread behind.
    #[test]
    fn halt_joins_the_worker_after_a_pause() {
        let fs = test_fs(Arc::new(PhysMap::new()));
        fs.start().expect("start");
        fs.pause();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs.is_quiesced() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fs.is_quiesced(), "worker did not park within its budget");

        fs.halt();
        assert!(fs.worker.lock().expect("worker lock").is_none());
        assert_eq!(fs.lifecycle_state(), Some(IndicatedState::Halt));
    }
}
