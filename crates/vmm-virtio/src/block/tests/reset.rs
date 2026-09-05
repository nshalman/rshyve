// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the device reset path: a reset must not return while a
//! worker can still write into the ring the driver is about to free.
//!
//! The transport publishes DEVICE_STATUS 0 as soon as `reset` returns,
//! and the driver frees the vring on reading that 0, so "`reset` has
//! not returned" and "the guest has not been told the reset succeeded"
//! are the same statement.

use std::sync::atomic::AtomicBool;

use super::*;

/// Longer than the two second budget the drain used to give up after.
/// A reset that returns inside this window has a deadline again.
const PAST_THE_OLD_DRAIN_BUDGET: Duration = Duration::from_millis(2500);

/// Take the shared side of `ring_access` the way a worker inside
/// `preadv` holds it, and put a request on the count so the reset
/// cannot take its idle short cut.
fn hold_ring_access(blk: &VirtioBlock) -> std::sync::RwLockReadGuard<'_, ()> {
    blk.ctx.inflight.fetch_add(1, Ordering::AcqRel);
    blk.ctx.ring_access.read().expect("ring access")
}

/// Block until the reset thread is inside the `ring_access` wait.
///
/// The epoch moves before the reset reaches the barrier and the caller
/// holds the shared side, so past this point the reset is in the wait
/// and can be nowhere else. Sleeping a fixed time instead would let the
/// assertions run after the reset had already finished.
fn wait_until_reset_blocks(
    blk: &VirtioBlock,
    epoch_before: u64,
    finished: &AtomicBool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while blk.ctx.epoch.load(Ordering::Acquire) == epoch_before
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(1));
    }
    assert_ne!(
        blk.ctx.epoch.load(Ordering::Acquire),
        epoch_before,
        "the reset never started"
    );
    assert!(
        !finished.load(Ordering::Acquire),
        "the reset finished while a worker still held ring access"
    );
}

/// Park every I/O worker, so a later dispatch is observable: the
/// requests sit in the worker channels instead of racing to completion.
fn park_workers(blk: &VirtioBlock) {
    blk.pause();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !blk.is_quiesced() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(blk.is_quiesced(), "workers did not park within the budget");
}

// A guest that writes 0 to DEVICE_STATUS frees the vring right after.
// A worker still holding a pre-reset chain must not run its I/O and must
// not publish into the used ring the guest has already recycled.
#[test]
fn reset_drops_requests_queued_before_it() {
    let (physmap, queue, blk) = make_get_id_queue(8, 5);
    let mut queues = [queue];

    park_workers(&blk);

    // Five requests is past the inline batch limit, so they dispatch.
    blk.notify_queue(0, &mut queues, &physmap);
    assert_eq!(
        queues[0].read_used_ring_idx(&physmap),
        0,
        "the parked workers must not have run yet"
    );

    VirtioDevice::reset(&blk);
    blk.resume();

    // The woken workers drop every stale request and decrement as they
    // go, so a count back at zero means all five have been dealt with.
    // Timing a window instead would pass on a runner that never
    // scheduled them.
    let deadline = Instant::now() + Duration::from_secs(5);
    while blk.ctx.inflight.load(Ordering::Acquire) != 0
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        blk.ctx.inflight.load(Ordering::Acquire),
        0,
        "the workers never finished with the pre-reset requests"
    );
    assert_eq!(
        queues[0].read_used_ring_idx(&physmap),
        0,
        "a worker published into the pre-reset used ring"
    );
}

// The reset waits on `ring_access`, never on the pause gate. It must
// leave the workers runnable, or the next driver's first request never
// completes.
#[test]
fn reset_leaves_the_workers_runnable() {
    let (physmap, queue, blk) = make_get_id_queue(8, 5);
    let mut queues = [queue];

    VirtioDevice::reset(&blk);
    assert!(!blk.gate.is_paused(), "reset left the workers parked");

    blk.notify_queue(0, &mut queues, &physmap);

    let deadline = Instant::now() + Duration::from_secs(5);
    while queues[0].read_used_ring_idx(&physmap) != 5
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        queues[0].read_used_ring_idx(&physmap),
        5,
        "post-reset requests never completed"
    );
}

// A reset raised while the device is already paused for migration must
// leave it paused: resuming here would let workers run behind the
// migration's back.
#[test]
fn reset_keeps_a_migration_pause() {
    let (physmap, queue, blk) = make_get_id_queue(8, 5);
    let mut queues = [queue];

    park_workers(&blk);
    // Five requests is past the inline batch limit, so they dispatch
    // and leave the count non-zero. Without that the reset takes the
    // idle short cut and never reaches the wait this covers.
    blk.notify_queue(0, &mut queues, &physmap);
    assert_ne!(
        blk.ctx.inflight.load(Ordering::Acquire),
        0,
        "the requests must still be outstanding when the reset runs"
    );

    VirtioDevice::reset(&blk);

    assert!(blk.gate.is_paused(), "reset cleared a migration pause");
    assert!(blk.is_quiesced());
}

// A reset installs a fresh VirtioCompletion, so a batch that spans one
// would publish live completions through the ring the guest just freed.
// The worker must hand the request back and start a new batch instead.
//
// The batch drain is the half the top of the worker loop does not cover:
// a stale request reached there would issue its syscall before anything
// looked at the epoch, and a read writes disk data straight into the
// pages the driver has recycled.
#[test]
fn a_worker_batch_does_not_span_a_reset() {
    const DESC_A: u64 = 0x1000;
    const AVAIL_A: u64 = 0x1100;
    const USED_A: u64 = 0x1200;
    const DESC_B: u64 = 0x1300;
    const AVAIL_B: u64 = 0x1400;
    const USED_B: u64 = 0x1500;
    const HDR_GPA: u64 = 0x2000;
    const STATUS_GPA: u64 = 0x2100;
    /// Stands in for a page the driver freed and the guest reused.
    const RECYCLED_GPA: u64 = 0x2200;
    const SECTOR: usize = 512;

    let physmap =
        Arc::new(PhysMap::new_anon(0x1000, 0x2000).expect("queue memory"));
    physmap
        .lookup(HDR_GPA, BLK_REQ_HEADER_SIZE)
        .expect("mapped request header")
        .write(&VirtioBlkReqHdr {
            rtype: bits::VIRTIO_BLK_T_FLUSH,
            _reserved: 0,
            sector: 0,
        })
        .expect("write request header");

    let ring = |desc, avail, used| {
        let mut queue = VirtQueue::new(8);
        queue.set_addr_modern(desc, avail, used);
        let completion =
            VirtioCompletion::new(&queue, Arc::clone(&physmap), || {});
        (queue, completion)
    };
    let (live_queue, live) = ring(DESC_A, AVAIL_A, USED_A);
    let (stale_queue, stale) = ring(DESC_B, AVAIL_B, USED_B);

    let chain = || {
        vec![
            ChainBuf::Readable {
                addr: HDR_GPA,
                len: BLK_REQ_HEADER_SIZE as u32,
            },
            ChainBuf::Writable {
                addr: STATUS_GPA,
                len: 1,
            },
        ]
    };
    let request =
        |head, completion: &Arc<VirtioCompletion>, epoch| BlkIoRequest {
            rtype: bits::VIRTIO_BLK_T_FLUSH,
            sector: 0,
            chain: chain(),
            head,
            completion: Arc::clone(completion),
            epoch,
        };
    // The stale one is a read, so running it is visible in guest memory
    // rather than only on the used ring.
    let stale_read =
        |head, completion: &Arc<VirtioCompletion>, epoch| BlkIoRequest {
            rtype: bits::VIRTIO_BLK_T_IN,
            sector: 0,
            chain: vec![
                ChainBuf::Readable {
                    addr: HDR_GPA,
                    len: BLK_REQ_HEADER_SIZE as u32,
                },
                ChainBuf::Writable {
                    addr: RECYCLED_GPA,
                    len: SECTOR as u32,
                },
                ChainBuf::Writable {
                    addr: STATUS_GPA,
                    len: 1,
                },
            ],
            head,
            completion: Arc::clone(completion),
            epoch,
        };

    // Epoch 1 is current; the middle request predates the reset.
    let ctx = WorkerCtx {
        physmap: Arc::clone(&physmap),
        gate: Arc::new(QuiesceGate::new(1)),
        epoch: Arc::new(AtomicU64::new(1)),
        inflight: Arc::new(AtomicUsize::new(3)),
        ring_access: Arc::new(RwLock::new(())),
    };
    let (tx, rx) = mpsc::channel();
    tx.send(request(0, &live, 1)).expect("send live");
    tx.send(stale_read(1, &stale, 0)).expect("send stale");
    tx.send(request(2, &live, 1)).expect("send live");
    drop(tx);

    // Disk content the stale read would land in the recycled page.
    let mut file = tempfile::tempfile().expect("create tempfile");
    std::io::Write::write_all(&mut file, &[0xAB; SECTOR])
        .expect("fill the backing file");
    let fd = file.as_raw_fd();
    let limits = DiskLimits {
        capacity: 1,
        read_only: true,
        nodelete: true,
    };
    let worker = {
        let ctx = ctx.clone();
        thread::spawn(move || blk_io_worker(rx, fd, limits, ctx))
    };
    worker
        .join()
        .expect("worker exits when the channel disconnects");

    assert_eq!(
        ctx.inflight.load(Ordering::Acquire),
        0,
        "every request must clear the in-flight count, dropped ones too"
    );

    assert_eq!(
        stale_queue.read_used_ring_idx(&physmap),
        0,
        "a stale request reached the pre-reset used ring"
    );
    assert_eq!(
        live_queue.read_used_ring_idx(&physmap),
        2,
        "the requests either side of the stale one must both complete"
    );

    let mut recycled = [0u8; SECTOR];
    physmap
        .lookup(RECYCLED_GPA, SECTOR)
        .expect("mapped recycled page")
        .read_bytes(&mut recycled)
        .expect("read the recycled page");
    assert_eq!(
        recycled, [0u8; SECTOR],
        "the stale read ran and wrote disk data into a recycled page"
    );
}

// A guest resets its virtio devices several times while it boots, and
// a worker takes 50ms to reach its park point. The reset waits on
// `ring_access`, which a worker holds only while it is using the ring,
// so it must never park the workers to find that out.
#[test]
fn reset_never_parks_the_workers() {
    let (physmap, queue, blk) = make_get_id_queue(8, 5);
    let mut queues = [queue];

    blk.notify_queue(0, &mut queues, &physmap);
    let deadline = Instant::now() + Duration::from_secs(5);
    while blk.ctx.inflight.load(Ordering::Acquire) != 0
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        blk.ctx.inflight.load(Ordering::Acquire),
        0,
        "the dispatched requests never finished"
    );

    VirtioDevice::reset(&blk);

    assert_eq!(
        blk.gate.drain_count(),
        0,
        "reset parked the workers to drain them"
    );
    assert!(!blk.gate.is_paused(), "reset left the workers parked");
}

// `reset` skips the wait when the in-flight count reads zero, so the
// count has to cover the used-ring write as well as the syscall. A
// worker that drops its count first leaves a window where a reset
// returns, the driver frees the vring, and the worker then publishes
// into the pages the guest has recycled.
#[test]
fn a_worker_holds_its_count_until_the_used_entry_lands() {
    let (physmap, queue, blk) = make_get_id_queue(8, 1);
    assert_eq!(blk.ctx.inflight.load(Ordering::Acquire), 0);

    // Read by the completion's interrupt, which runs after the used
    // entry and the used index are both written.
    let at_publish = Arc::new(AtomicUsize::new(usize::MAX));
    let completion = {
        let seen = Arc::clone(&at_publish);
        let inflight = Arc::clone(&blk.ctx.inflight);
        VirtioCompletion::new(&queue, Arc::clone(&physmap), move || {
            seen.store(inflight.load(Ordering::Acquire), Ordering::Release);
        })
    };
    completion.set_force_interrupt(1);

    let (tx, rx) = mpsc::channel();
    blk.ctx.inflight.fetch_add(1, Ordering::AcqRel);
    tx.send(BlkIoRequest {
        rtype: bits::VIRTIO_BLK_T_FLUSH,
        sector: 0,
        chain: Vec::new(),
        head: 0,
        completion,
        epoch: blk.ctx.epoch.load(Ordering::Acquire),
    })
    .expect("queue the request");
    drop(tx);

    let ctx = blk.ctx.clone();
    let fd = blk.fd;
    let limits = DiskLimits {
        capacity: blk.capacity,
        read_only: blk.read_only,
        nodelete: blk.nodelete,
    };
    thread::spawn(move || blk_io_worker(rx, fd, limits, ctx))
        .join()
        .expect("worker");

    assert_eq!(
        at_publish.load(Ordering::Acquire),
        1,
        "the count must still cover the request when its used entry lands",
    );
    assert_eq!(
        queue.read_used_ring_idx(&physmap),
        1,
        "the used entry never reached the ring, so the callback proved \
         nothing",
    );
    assert_eq!(
        blk.ctx.inflight.load(Ordering::Acquire),
        0,
        "the worker left its request on the count",
    );
}

// The epoch has to be read before the syscall, not after it. A read
// that runs against a stale chain writes disk contents straight into
// the pages the driver has already recycled; suppressing the used
// entry afterwards does not take that back.
#[test]
fn a_stale_request_does_not_run_its_syscall() {
    const HDR_GPA: u64 = 0x2000;
    const STATUS_GPA: u64 = 0x2100;
    /// Stands in for a page the driver freed and the guest reused.
    const RECYCLED_GPA: u64 = 0x2200;
    const SECTOR: usize = 512;

    let physmap =
        Arc::new(PhysMap::new_anon(0x1000, 0x2000).expect("queue memory"));
    let mut queue = VirtQueue::new(8);
    queue.set_addr_modern(0x1000, 0x1100, 0x1200);
    let completion = VirtioCompletion::new(&queue, Arc::clone(&physmap), || {});

    let mut file = tempfile::tempfile().expect("create tempfile");
    std::io::Write::write_all(&mut file, &[0xAB; SECTOR])
        .expect("fill the backing file");
    let limits = DiskLimits {
        capacity: 1,
        read_only: true,
        nodelete: true,
    };
    // Epoch 1 is current, so the request dispatched under 0 is stale.
    let ctx = WorkerCtx {
        physmap: Arc::clone(&physmap),
        gate: Arc::new(QuiesceGate::new(1)),
        epoch: Arc::new(AtomicU64::new(1)),
        inflight: Arc::new(AtomicUsize::new(1)),
        ring_access: Arc::new(RwLock::new(())),
    };

    let done = process_one_request(
        file.as_raw_fd(),
        &limits,
        &ctx,
        BlkIoRequest {
            rtype: bits::VIRTIO_BLK_T_IN,
            sector: 0,
            chain: vec![
                ChainBuf::Readable {
                    addr: HDR_GPA,
                    len: BLK_REQ_HEADER_SIZE as u32,
                },
                ChainBuf::Writable {
                    addr: RECYCLED_GPA,
                    len: SECTOR as u32,
                },
                ChainBuf::Writable {
                    addr: STATUS_GPA,
                    len: 1,
                },
            ],
            head: 0,
            completion,
            epoch: 0,
        },
    );

    assert!(done.is_none(), "a stale request must publish nothing");
    let mut recycled = [0u8; SECTOR];
    physmap
        .lookup(RECYCLED_GPA, SECTOR)
        .expect("mapped recycled page")
        .read_bytes(&mut recycled)
        .expect("read the recycled page");
    assert_eq!(
        recycled, [0u8; SECTOR],
        "the stale read ran and wrote disk data into a recycled page"
    );
}

// The window codex found in the budgeted drain: a worker passes the
// epoch check, is descheduled inside `preadv`, the wait gives up, and
// the reset returns. The guest reads DEVICE_STATUS as 0, frees the
// vring, and the read lands in the recycled pages. Checking the epoch
// afterwards cannot take that write back, so the reset must not finish
// at all while a worker holds the shared side of `ring_access`.
#[test]
fn a_reset_waits_out_a_worker_that_never_finishes_in_time() {
    let (_physmap, _queue, blk) = make_get_id_queue(8, 1);

    let access = hold_ring_access(&blk);
    let epoch_before = blk.ctx.epoch.load(Ordering::Acquire);
    let finished = AtomicBool::new(false);
    let blk_ref = &blk;
    let finished_ref = &finished;

    thread::scope(|scope| {
        scope.spawn(move || {
            VirtioDevice::reset(blk_ref);
            finished_ref.store(true, Ordering::Release);
        });

        wait_until_reset_blocks(&blk, epoch_before, &finished);

        let until = Instant::now() + PAST_THE_OLD_DRAIN_BUDGET;
        while Instant::now() < until {
            assert!(
                !finished.load(Ordering::Acquire),
                "the reset told the guest it was done while a worker \
                 could still write into the ring"
            );
            thread::sleep(Duration::from_millis(10));
        }

        drop(access);
    });

    assert!(
        finished.load(Ordering::Acquire),
        "the reset never finished after the worker let the ring go"
    );
}
