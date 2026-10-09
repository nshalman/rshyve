// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The virtio-blk I/O workers.
//!
//! Every request is bounced through host memory. A worker holds
//! guest-access permission across memory copies and ring writes only,
//! never across a call to the backing store, so a device reset waits
//! for a bounded amount of work and can run on the vCPU.

use std::fs::File;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use vmm_core::mem::PhysMap;
use vmm_devices::blkdev::{BackingDisk, DiskSync};
use vmm_devices::QuiesceGate;

use super::access::BackendBarrier;
use super::budget::{Budget, Charge};
use super::io::{self, BOUNCE_BYTES};
use super::{bits, discard, probes, ChainBuf, DiskLimits};
use crate::access::{GuestAccess, RingTag};
use crate::pci::notify::KickSlot;
use crate::queue::VirtioCompletion;

/// Completions a worker gathers before it takes the `VirtioCompletion`
/// lock once for the whole set. Only results that have already
/// finished are batched, so nothing waits on the disk behind them.
const BATCH_MAX: usize = 16;

/// I/O request sent to worker threads.
pub(super) struct BlkIoRequest {
    pub(super) rtype: u32,
    pub(super) sector: u64,
    pub(super) chain: Vec<ChainBuf>,
    pub(super) head: u16,
    pub(super) completion: Arc<VirtioCompletion>,
    /// Ring and generation this request was dispatched under. A worker
    /// drops the request when it no longer matches: the chain and the
    /// completion both point at a ring the driver has since torn down.
    pub(super) tag: RingTag,
    /// Budget the request holds until its answer is published.
    pub(super) charge: Charge,
}

/// A one-shot hook a test installs to pin an interleaving.
#[cfg(test)]
pub(super) type ParkSlot =
    std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>;

/// Park points a test uses to hold a request at a chosen step.
#[cfg(test)]
#[derive(Default)]
pub(super) struct WorkerParks {
    /// Reached where a request is about to call the backing store,
    /// with no guest-access permission held.
    pub(super) at_backend: ParkSlot,
    /// Reached at the same point by every request, and never cleared.
    /// Stands in for a slow disk.
    pub(super) each_backend: ParkSlot,
    /// Reached with the guest side of a pass finished and its used
    /// entries not yet published.
    pub(super) before_publish: ParkSlot,
    /// Reached with the used entries published and the interrupt for
    /// them not yet raised.
    pub(super) before_raise: ParkSlot,
    /// Reached on the vCPU with requests taken from the ring and not
    /// yet handed to a worker.
    pub(super) before_dispatch: ParkSlot,
    /// While set, a parked worker leaves work queued to it, as one
    /// that has not looked yet does.
    pub(super) parked_skips_work: std::sync::atomic::AtomicBool,
}

/// Run a park point, if a test installed one, and clear it.
#[cfg(test)]
pub(super) fn run_park(slot: &ParkSlot) {
    let hook = slot.lock().expect("park lock poisoned").take();
    if let Some(hook) = hook {
        hook();
    }
}

/// Run a hook that stays installed, if a test installed one.
#[cfg(test)]
fn run_each(slot: &ParkSlot) {
    let hook = slot.lock().expect("park lock poisoned").clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// State every I/O worker shares with the device.
#[derive(Clone)]
pub(super) struct WorkerCtx {
    pub(super) physmap: Arc<PhysMap>,
    pub(super) gate: Arc<QuiesceGate>,
    /// Permission to read and write the guest's ring and chains.
    pub(super) access: Arc<GuestAccess>,
    /// Keeps a write admitted before a reset from overtaking one the
    /// next driver sends to the same sector.
    pub(super) backend: Arc<BackendBarrier>,
    /// Work the device has taken from its rings and not finished.
    pub(super) budget: Arc<Budget>,
    /// Drains a ring that gave chains back for want of budget.
    pub(super) kick: Arc<KickSlot>,
    #[cfg(test)]
    pub(super) parks: Arc<WorkerParks>,
}

/// The host memory one worker reuses for every request.
///
/// Retained until the syscall that uses it returns, so a reset never
/// reclaims a buffer an operation still owns. It grows to at most
/// [`BOUNCE_BYTES`], and only with no permission held, so a request
/// never allocates inside a guest-access section.
pub(super) struct Bounce {
    buf: Vec<u8>,
}

impl Bounce {
    pub(super) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn take(&mut self, want: usize) -> &mut [u8] {
        let n = want.min(BOUNCE_BYTES);
        if self.buf.len() < n {
            self.buf.resize(n, 0);
        }
        &mut self.buf[..n]
    }
}

/// Whether an opcode reaches the backing store.
///
/// One that does must run on a worker. Inline it would hold the
/// transport lock for as long as the disk takes, and a reset waits
/// behind that lock.
pub(super) fn needs_backend(rtype: u32) -> bool {
    matches!(
        rtype,
        bits::VIRTIO_BLK_T_IN
            | bits::VIRTIO_BLK_T_OUT
            | bits::VIRTIO_BLK_T_FLUSH
            | bits::VIRTIO_BLK_T_DISCARD
            | bits::VIRTIO_BLK_T_WRITE_ZEROES
    )
}

/// Serve one queue's requests until the device drops its sender.
///
/// The worker owns a reference to the backing store for its whole
/// life, so a syscall that is still running when the device goes away
/// cannot land on a descriptor the host has reopened.
pub(super) fn blk_io_worker(
    rx: mpsc::Receiver<BlkIoRequest>,
    disk: Arc<BackingDisk>,
    limits: DiskLimits,
    ctx: WorkerCtx,
) {
    use std::sync::mpsc::RecvTimeoutError;

    let mut bounce = Bounce::new();
    let mut done = Pass::new();
    // Carries the one request a batch drain had to hand back because it
    // crossed a reset boundary.
    let mut carried: Option<BlkIoRequest> = None;

    loop {
        let req = match carried.take() {
            Some(req) => req,
            None => match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => {
                    carried = ctx.gate.park_if_paused_or(|| {
                        #[cfg(test)]
                        if ctx
                            .parks
                            .parked_skips_work
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            return None;
                        }
                        rx.try_recv().ok()
                    });
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };

        // `notify_queue` dispatches nothing before the store opens, and
        // an open store stays open.
        let Some(disk) = disk.get() else {
            continue;
        };
        run_batch(
            &rx,
            disk,
            &limits,
            &ctx,
            &mut bounce,
            &mut done,
            req,
            &mut carried,
        );
    }
}

/// The results of one pass, and the budget its requests hold until
/// those results are published.
///
/// A request's budget is what tells a pause the request is still
/// owed. Given back before the publish, a pause could see the device
/// quiet while an answer is still missing from the used ring.
struct Pass {
    entries: Vec<(u16, u32)>,
    charges: Vec<Charge>,
}

impl Pass {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(BATCH_MAX),
            charges: Vec::with_capacity(BATCH_MAX),
        }
    }
}

/// Run one pass of requests and publish the results together.
///
/// Every request in a pass belongs to one driver session and one
/// virtqueue, so a reset ends the pass rather than splitting it. The
/// pass takes guest-access permission several times and holds it
/// across no call to the disk.
#[allow(clippy::too_many_arguments)]
fn run_batch(
    rx: &mpsc::Receiver<BlkIoRequest>,
    disk: &DiskSync,
    limits: &DiskLimits,
    ctx: &WorkerCtx,
    bounce: &mut Bounce,
    done: &mut Pass,
    first: BlkIoRequest,
    carried: &mut Option<BlkIoRequest>,
) {
    let completion = Arc::clone(&first.completion);
    let tag = first.tag;
    done.entries.clear();

    let mut next = Some(first);
    let mut count = 0usize;
    while let Some(mut req) = next.take() {
        count += 1;
        if let Some(entry) = run_request(disk, limits, ctx, bounce, &req) {
            done.entries.push(entry);
        }
        done.charges.push(std::mem::take(&mut req.charge));
        if count >= BATCH_MAX {
            break;
        }
        match rx.try_recv() {
            // A reset installs a fresh completion, so a request from
            // the other side of one must start its own pass.
            Ok(more)
                if more.tag == tag
                    && Arc::ptr_eq(&more.completion, &completion) =>
            {
                next = Some(more)
            }
            Ok(more) => {
                *carried = Some(more);
                break;
            }
            Err(_) => break,
        }
    }

    publish(ctx, &completion, tag, &done.entries);
    done.charges.clear();
    // The release is what marks starved rings due, so it goes first. The
    // kick takes chains only through admission, which a pause has closed.
    kick_starved(ctx);
}

/// Drain the rings that wait for the budget this pass gave back.
///
/// Runs with no guest-access permission held: a kick takes the
/// transport lock, and a ring reprogram waits for permission while it
/// holds that lock.
fn kick_starved(ctx: &WorkerCtx) {
    for queue_idx in ctx.budget.take_kicks() {
        ctx.kick.kick(queue_idx);
    }
}

/// Publish the finished results, then raise the interrupt outside the
/// permission that wrote them.
fn publish(
    ctx: &WorkerCtx,
    completion: &Arc<VirtioCompletion>,
    tag: RingTag,
    done: &[(u16, u32)],
) {
    if done.is_empty() {
        return;
    }
    #[cfg(test)]
    run_park(&ctx.parks.before_publish);
    let raise = {
        // A reset between the last copy and here takes the ring away,
        // so the results go nowhere.
        let Some(_session) = ctx.access.enter(tag) else {
            return;
        };
        completion.publish_batch(done)
    };
    #[cfg(test)]
    run_park(&ctx.parks.before_raise);
    if raise {
        ctx.access.deliver(tag, || completion.signal());
    }
}

/// Run one request and return its used-ring entry.
///
/// `None` means the request belongs to a session that has ended. It
/// publishes nothing and writes nothing into guest memory.
pub(super) fn run_request(
    disk: &DiskSync,
    limits: &DiskLimits,
    ctx: &WorkerCtx,
    bounce: &mut Bounce,
    req: &BlkIoRequest,
) -> Option<(u16, u32)> {
    let file: &File = disk.file();
    let (status, used_len) = match req.rtype {
        bits::VIRTIO_BLK_T_IN => read_request(file, ctx, bounce, req)?,
        bits::VIRTIO_BLK_T_OUT if !limits.read_only => {
            write_request(file, ctx, bounce, req)?
        }
        bits::VIRTIO_BLK_T_OUT => (bits::VIRTIO_BLK_S_IOERR, 1),
        bits::VIRTIO_BLK_T_FLUSH => flush_request(disk, ctx, req)?,
        bits::VIRTIO_BLK_T_DISCARD | bits::VIRTIO_BLK_T_WRITE_ZEROES => {
            dwz_request(file, limits, ctx, req)?
        }
        _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
    };

    let _session = ctx.access.enter(req.tag)?;
    io::write_status(&ctx.physmap, &req.chain, status);
    probes::vioblk_complete!(|| (req.head, status));
    Some((req.head, used_len))
}

/// The budget does not bound a request past it, which may move 1 GiB.
/// A pause lets it run for the grace, then stops it with its charge
/// held, so the device stays not quiet and that pause fails.
fn hold_between_chunks(ctx: &WorkerCtx, req: &BlkIoRequest, moved: usize) {
    if moved != 0 && req.charge.is_alone() {
        ctx.gate.hold_in_transfer();
    }
}

/// Read from the disk into host memory, then copy that into the guest
/// under permission.
///
/// The copy is the only guest-memory step, and it is bounded by
/// [`BOUNCE_BYTES`]. A read that stalls holds nothing a reset waits
/// for.
fn read_request(
    file: &File,
    ctx: &WorkerCtx,
    bounce: &mut Bounce,
    req: &BlkIoRequest,
) -> Option<(u8, u32)> {
    let (Some(total), Some(base)) = (
        io::transfer(&req.chain, true),
        io::sector_offset(req.sector),
    ) else {
        return Some((bits::VIRTIO_BLK_S_IOERR, 1));
    };
    if total == 0 {
        return Some((bits::VIRTIO_BLK_S_OK, 1));
    }

    let mut moved = 0usize;
    while moved < total {
        hold_between_chunks(ctx, req, moved);
        let buf = bounce.take(total - moved);
        let n = buf.len();
        let Some(at) = base.checked_add(moved as u64) else {
            return Some((bits::VIRTIO_BLK_S_IOERR, 1));
        };
        {
            let _ticket = ctx.backend.admit(&ctx.access, req.tag, false)?;
            #[cfg(test)]
            {
                run_park(&ctx.parks.at_backend);
                run_each(&ctx.parks.each_backend);
            }
            // A short read or an error leaves the tail of the buffer
            // holding the last request, so nothing is copied out.
            if !io::read_backend(file, at, buf) {
                return Some((bits::VIRTIO_BLK_S_IOERR, 1));
            }
        }
        let _session = ctx.access.enter(req.tag)?;
        if !io::scatter(&ctx.physmap, &req.chain, moved, buf) {
            return Some((bits::VIRTIO_BLK_S_IOERR, 1));
        }
        moved += n;
    }
    let len = u32::try_from(total).unwrap_or(u32::MAX).saturating_add(1);
    Some((bits::VIRTIO_BLK_S_OK, len))
}

/// Snapshot the payload under permission, then write the snapshot.
///
/// The snapshot is what takes the guest out of the write. A `pwrite`
/// that stalls across a reset resumes against host memory, so it
/// cannot pick up whatever the driver put in the recycled page.
fn write_request(
    file: &File,
    ctx: &WorkerCtx,
    bounce: &mut Bounce,
    req: &BlkIoRequest,
) -> Option<(u8, u32)> {
    let (Some(total), Some(base)) = (
        io::transfer(&req.chain, false),
        io::sector_offset(req.sector),
    ) else {
        return Some((bits::VIRTIO_BLK_S_IOERR, 1));
    };
    if total == 0 {
        return Some((bits::VIRTIO_BLK_S_OK, 1));
    }

    let mut moved = 0usize;
    while moved < total {
        hold_between_chunks(ctx, req, moved);
        let buf = bounce.take(total - moved);
        let n = buf.len();
        {
            let _session = ctx.access.enter(req.tag)?;
            // A partial snapshot would put the last request's bytes on
            // the disk, so the write is abandoned instead.
            if !io::gather(&ctx.physmap, &req.chain, moved, buf) {
                return Some((bits::VIRTIO_BLK_S_IOERR, 1));
            }
        }
        let Some(at) = base.checked_add(moved as u64) else {
            return Some((bits::VIRTIO_BLK_S_IOERR, 1));
        };
        let _ticket = ctx.backend.admit(&ctx.access, req.tag, true)?;
        #[cfg(test)]
        {
            run_park(&ctx.parks.at_backend);
            run_each(&ctx.parks.each_backend);
        }
        if !io::write_backend(file, at, buf) {
            return Some((bits::VIRTIO_BLK_S_IOERR, 1));
        }
        moved += n;
    }
    Some((bits::VIRTIO_BLK_S_OK, 1))
}

fn flush_request(
    disk: &DiskSync,
    ctx: &WorkerCtx,
    req: &BlkIoRequest,
) -> Option<(u8, u32)> {
    // A flush commits what is already on the disk, so a stale one
    // cannot put the wrong bytes in a sector. It waits for the older
    // writes but does not hold the next session behind itself.
    let _ticket = ctx.backend.admit(&ctx.access, req.tag, false)?;
    if disk.sync().is_ok() {
        Some((bits::VIRTIO_BLK_S_OK, 1))
    } else {
        Some((bits::VIRTIO_BLK_S_IOERR, 1))
    }
}

fn dwz_request(
    file: &File,
    limits: &DiskLimits,
    ctx: &WorkerCtx,
    req: &BlkIoRequest,
) -> Option<(u8, u32)> {
    let plan = {
        let _session = ctx.access.enter(req.tag)?;
        discard::plan(limits, req.rtype, &req.chain, &ctx.physmap)
    };
    match plan {
        discard::Plan::Settled(status) => Some((status, 1)),
        discard::Plan::Zero(ranges) => {
            let _ticket = ctx.backend.admit(&ctx.access, req.tag, true)?;
            Some((discard::zero(file, &ranges), 1))
        }
    }
}
