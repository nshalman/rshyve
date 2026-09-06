// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reset measured where the guest sees it, not at the drain.
//!
//! `GuestAccess::drain` is one step of `VirtioDevice::reset`, and a
//! test pointed at it cannot see a wait the reset does after it. That
//! is exactly where the wait used to be: a review timed the whole
//! reset against a parked interrupt and got 1.000191292s where the
//! drain alone measured nothing. Every measurement here brackets the
//! whole call.

use super::*;

/// A reset that takes longer than this waited for something other
/// than a bounded memory copy.
const WHOLE_RESET_BUDGET: Duration = Duration::from_secs(1);

/// Run `reset` on its own thread and return how long it took.
///
/// The reset is the thing under test, so a broken one waits for a park
/// only this thread can release. Running it here and releasing after
/// the deadline turns that into a measurement and a failed assertion
/// instead of a hung run.
fn time_reset(fs: &VirtioFs, release: &dyn Fn()) -> Duration {
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| VirtioDevice::reset(fs));
        let start = Instant::now();
        while !worker.is_finished() && start.elapsed() < WHOLE_RESET_BUDGET {
            std::thread::sleep(Duration::from_micros(20));
        }
        let waited = start.elapsed();
        release();
        waited
    })
}

// The measurement the last review used to prove the reset unbounded,
// repeated at the same boundary. A worker is held inside the interrupt
// callback, standing in for an injection the kernel has not finished.
// The reset must not queue behind it: the illumos driver reclaims the
// ring the moment its one register write returns.
#[test]
fn a_reset_does_not_wait_for_interrupt_injection() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let hold = Arc::new(AtomicBool::new(true));
    let (_raised, inside) = counting_interrupt(&fs, &hold);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    let deadline = Instant::now() + PARK_BUDGET;
    while !inside.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        inside.load(Ordering::Acquire),
        "the worker never raised the interrupt"
    );

    let waited = time_reset(&fs, &|| hold.store(false, Ordering::Release));

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "the reset waited {waited:?} behind an interrupt injection"
    );
}

// A guest picks how much stale work is waiting when it writes 0 to
// DEVICE_STATUS. A full queue of it must not lengthen the reset by so
// much as one request: admission shuts first, and work refused there
// never touches the lock the resetting vCPU waits for.
#[test]
fn a_full_queue_of_stale_work_does_not_lengthen_the_reset() {
    const BACKLOG: u16 = FS_QUEUE_SIZE_DEFAULT;
    let (physmap, mut queues, fs) = ring(BACKLOG, BACKLOG);
    fs.start().expect("start");

    // Park the worker so a whole queue piles up behind it instead of
    // completing one by one.
    park_the_worker(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        usize::from(BACKLOG),
        "the backlog never reached the worker"
    );
    fs.resume();

    let waited = time_reset(&fs, &|| ());
    let req = usize::from(FS_REQUEST_QUEUE);
    let published = queues[req].read_used_ring_idx(&physmap);

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "a backlog of {BACKLOG} held the resetting vCPU for {waited:?}"
    );
    // The rest of the backlog is still running. None of it may reach
    // the ring the guest is free to reclaim now the write has
    // returned, so the index has to stand still from here.
    assert!(
        published < BACKLOG,
        "the whole backlog finished before the reset"
    );
    std::thread::sleep(BACKLOG_BUDGET);
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        published,
        "a stale request published into the ring after the reset returned"
    );
}

// The backing store is the one thing a reset can never wait for: a
// host filesystem that stops answering must not be able to hold a
// vCPU. Measured at the whole reset for the same reason as the rest.
#[test]
fn a_parked_backing_store_does_not_lengthen_the_reset() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let park = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    park.wait_until_inside();

    let waited = time_reset(&fs, &|| park.release());

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "the reset waited {waited:?} behind the backing store"
    );
}

// The notify path runs on the vCPU under the transport's register
// lock, and a reset on another vCPU has to take that lock before it
// can close admission. So an injection here is one a guest can put in
// front of a reset. The heads this path answers itself go back to the
// transport, which drops the lock first.
#[test]
fn the_notify_path_hands_the_interrupt_to_the_transport() {
    let (physmap, mut queues, fs) = ring(8, 1);
    // A chain that runs off the end of the descriptor table, so the
    // walk answers the head itself instead of sending it to a worker.
    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped request descriptor")
        .write(&VirtqDesc {
            addr: REQ_GPA,
            len: 64,
            flags: crate::bits::VRING_DESC_F_NEXT,
            next: 8,
        })
        .expect("write request descriptor");
    fs.start().expect("start");

    let raised = Arc::new(AtomicBool::new(false));
    {
        let raised = Arc::clone(&raised);
        *fs.interrupt.lock().expect("interrupt lock") =
            Some(Arc::new(move |_queue: u16| {
                raised.store(true, Ordering::Release)
            }));
    }

    let asked = fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    assert_eq!(
        queues[usize::from(FS_REQUEST_QUEUE)].read_used_ring_idx(&physmap),
        1,
        "the head the walk answered never reached the used ring"
    );
    assert!(asked, "the transport was not asked to raise the interrupt");
    assert!(
        !raised.load(Ordering::Acquire),
        "the notify path injected an interrupt under the transport lock"
    );
}
