// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reset measured where the guest sees it, not at the drain.
//!
//! `GuestAccess::drain` is one step of `VirtioDevice::reset`, so a test
//! pointed at the drain cannot see a wait the reset does after it, and
//! that is where an unbounded wait hides. Every measurement here
//! brackets the whole call.

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

// A worker is held inside the interrupt
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
// much as one request, and none of it may reach the ring afterwards:
// the driver is free to reclaim that memory the moment the write
// returns.
#[test]
fn a_full_queue_of_stale_work_does_not_lengthen_the_reset() {
    const BACKLOG: u16 = FS_QUEUE_SIZE_DEFAULT;
    let (physmap, mut queues, fs) = ring(BACKLOG, BACKLOG);
    let req = usize::from(FS_REQUEST_QUEUE);
    fs.start().expect("start");

    // Park the worker so a whole queue piles up behind it instead of
    // completing one by one, which is what a guest arranges before it
    // writes the reset register.
    park_the_worker(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        usize::from(BACKLOG),
        "the backlog never reached the worker"
    );

    let waited = time_reset(&fs, &|| ());

    // Let the worker loose on the backlog the reset ended. Every
    // request in it names the session that is gone.
    fs.resume();
    let deadline = Instant::now() + BACKLOG_BUDGET;
    while fs.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "a backlog of {BACKLOG} held the resetting vCPU for {waited:?}"
    );
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "the backlog never retired"
    );
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "a stale request published into the ring the reset gave back"
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
        fs.interrupt
            .install(BackendIntr::detached(move |_session, _queue| {
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

// virtio-fs keeps the same two counters as virtio-blk, and the gap
// between them is the same. The worker's epoch check is one load, and
// a whole reset can run after it passes. What keeps a reply off the
// next driver is the session the completion handler was built in,
// which the transport tests in the same step that admits the delivery.
#[test]
fn a_completion_from_the_closed_session_raises_nothing() {
    let (_physmap, queues, fs) = ring(8, 0);
    let req = usize::from(FS_REQUEST_QUEUE);
    let gate = Arc::new(IntrGate::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    fs.interrupt
        .install(BackendIntr::recording(Arc::clone(&gate), Arc::clone(&seen)));

    // Built the way a queue notification builds it: under permission.
    let handler = || {
        let session = fs.access.enter_current(0).expect("a session is open");
        fs.get_completion(&session, FS_REQUEST_QUEUE, &queues[req])
    };
    let stale = handler();

    // The whole reset, in the order the transport runs it: the session
    // ends first, then the backend resets, then admission reopens.
    gate.end_session();
    VirtioDevice::reset(&fs);
    gate.reopen();

    stale.signal();
    assert!(
        seen.lock().expect("record lock").is_empty(),
        "a reply of the closed session interrupted the driver that followed"
    );

    // The other half, and it is the worse failure of the two: a reply
    // of the session running now has to get through, or the device has
    // traded a spurious interrupt for a guest that waits for ever.
    handler().signal();
    assert_eq!(
        seen.lock().expect("record lock").len(),
        1,
        "the driver that is running got no interrupt for its reply"
    );
}
