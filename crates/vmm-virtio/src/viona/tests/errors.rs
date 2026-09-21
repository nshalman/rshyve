// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the device does when the kernel refuses a call.
//!
//! Every call into the kernel can fail, and each failure has its own
//! branch here. The warning it writes is not what these tests read.
//! What matters is what the device does next: a ring marked ready
//! after a refused program is one the next kick sends to a kernel with
//! no worker for it, and a sweep that stops at the first failure is
//! one that never reaches the delete.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use vmm_devices::Lifecycle;

use super::device::{addressed_queue, device_with, notify, running_device};
use super::{cleared, null_log, steps_of, Op, Step, Steps, TestLink};
use crate::pci::intr::{BackendIntr, IntrSlot};
use crate::viona::halt::halt_link;
use crate::viona::poll::{deliver_pending, viona_intr_poll_loop};
use crate::viona::{
    RingState, NET_NUM_QUEUES, NET_QUEUE_SIZE, VIRTIO_NET_F_MAC,
};
use crate::VirtioDevice;

/// A wait this long means a thread is wedged, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// The ring the restore tests program.
const RESTORED_RING: u16 = 1;

/// Where the guest put that ring.
const DESC: u64 = 0x1_0000;
const AVAIL: u64 = 0x2_0000;
const USED: u64 = 0x3_0000;

/// The message a migrated driver had programmed for it.
const MSG_ADDR: u64 = 0xFEE0_0000;
const MSG_DATA: u32 = 0x21;

/// The state one restore programs.
fn restored_state() -> viona_api::vioc_ring_state {
    viona_api::vioc_ring_state {
        vrs_index: RESTORED_RING,
        vrs_avail_idx: 7,
        vrs_used_idx: 5,
        vrs_qsize: NET_QUEUE_SIZE,
        vrs_qaddr_desc: DESC,
        vrs_qaddr_avail: AVAIL,
        vrs_qaddr_used: USED,
    }
}

/// Restore one ring, the way a migration destination does.
fn restore(device: &crate::viona::VirtioViona) {
    VirtioDevice::restore_ring_state(
        device,
        RESTORED_RING,
        NET_QUEUE_SIZE,
        DESC,
        AVAIL,
        USED,
        7,
        5,
        MSG_ADDR,
        MSG_DATA,
    );
}

/// An interrupt path that records the rings it was raised for.
fn wired_interrupt() -> (IntrSlot, Arc<Mutex<Vec<u16>>>) {
    let raised = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&raised);
    let slot = IntrSlot::new();
    slot.install(BackendIntr::detached(move |_session, ring| {
        sink.lock().expect("raised lock").push(ring);
    }));
    (slot, raised)
}

#[test]
fn a_ring_the_kernel_refused_to_program_is_not_ready() {
    // The ready flag is what lets the next kick through. A ring the
    // kernel refused has no worker: the kick is refused at best, and
    // on an older viona it dereferences a NULL in the mac layer.
    // Userspace already validated these addresses, so writing them
    // again cannot help; the ring is quarantined until a device reset.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingInit]);
    let device = device_with(Arc::new(link));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());
    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init, RingState::Error],
        "a ring the kernel refused was marked ready",
    );
    assert_eq!(
        steps_of(&steps).len(),
        1,
        "a quarantined ring kept issuing ioctls, one per guest write",
    );
}

/// `viona_ioc_ring_reset` waits with signals heeded, so `EINTR` says
/// nothing about the ring, only that this thread took a signal. A reset
/// that believed it would quarantine a working NIC and leave the ring
/// the driver asked for unprogrammed.
#[test]
fn a_reset_a_signal_interrupted_is_tried_again() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).interrupted_on(Op::RingReset, 2);
    let device = running_device(Arc::new(link));

    VirtioDevice::queue_addr_set(&device, RESTORED_RING, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingReset(RESTORED_RING),
            Step::RingReset(RESTORED_RING),
            Step::RingReset(RESTORED_RING),
            Step::RingInit {
                ring: RESTORED_RING,
                size: NET_QUEUE_SIZE,
                desc: DESC,
                avail: AVAIL,
                used: USED,
            },
        ],
        "the reset gave up on a signal",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state
            [RESTORED_RING as usize],
        RingState::Ready,
    );
}

/// A `DEVICE_STATUS` write of 0 publishes status 0 as soon as the
/// backend returns, and an illumos driver reclaims the ring pages on
/// the next line. A kernel ring the reset could not stop keeps reading
/// and writing them, so it must not read back as unprogrammed:
/// `RING_INIT` over a running ring only fails with `EBUSY`, which would
/// leave the stale worker and the new addresses both in place.
#[test]
fn a_device_reset_the_kernel_refused_quarantines_the_ring() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingReset]);
    let device = running_device(Arc::new(link));

    VirtioDevice::reset(&device);

    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Error; NET_NUM_QUEUES],
        "a ring the kernel still runs read back as unprogrammed",
    );

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());
    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1)],
        "a quarantined ring was programmed over",
    );
}

/// The kernel refuses a kick for any ring it is not running, and a
/// guest can spin on `QUEUE_NOTIFY`, so one warning per refusal is a
/// host log flood at a rate the guest picks. The latch is what holds it
/// to one line per run of refusals.
///
/// The ring stays in service: a migration export pauses the kernel
/// rings and leaves this state at `Ready`, so a ring retired on a
/// refused kick would leave a dead NIC behind an aborted export.
#[test]
fn refused_kicks_warn_once_and_leave_the_ring_in_service() {
    const KICKS: usize = 8;
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingKick]);
    let device = running_device(Arc::new(link));

    for _ in 0..KICKS {
        notify(&device, 1);
    }

    let inner = device.inner.lock().expect("viona lock");
    assert!(
        inner.kick_refused[1],
        "the latch is open, so every later refusal writes its own line",
    );
    assert_eq!(
        inner.ring_state[1],
        RingState::Ready,
        "a refused kick took the ring out of service",
    );
    drop(inner);
    assert_eq!(steps_of(&steps).len(), KICKS);
}

#[test]
fn a_ring_state_the_kernel_refused_programs_nothing_after_it() {
    // The restore stops at the first refusal. Carrying on would mark
    // the ring ready, route an MSI-X message to a ring the kernel does
    // not have, and kick a worker that was never started.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingSetState]);
    let device = device_with(Arc::new(link));
    device.inner.lock().expect("viona lock").negotiated_features =
        u64::from(VIRTIO_NET_F_MAC);

    restore(&device);

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC)),
            Step::RingSetState(restored_state()),
        ],
        "the restore carried on past a ring the kernel refused",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init; NET_NUM_QUEUES],
        "a ring the kernel refused was marked ready",
    );
}

#[test]
fn a_message_the_kernel_refused_does_not_stop_the_restore() {
    // MSI-X is the kernel's shortcut, not the only route: without it
    // the poll thread still raises the interrupt. So the ring is ready
    // and kicked either way, and a restore that stopped here would
    // leave a migrated guest with a ring nobody kicks.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingSetMsi]);
    let device = running_device(Arc::new(link));

    restore(&device);

    assert_eq!(
        steps_of(&steps).last(),
        Some(&Step::RingKick(RESTORED_RING)),
        "a refused message left the restored ring unkicked",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state
            [RESTORED_RING as usize],
        RingState::Ready,
    );
}

#[test]
fn a_halt_deletes_the_link_though_every_ring_reset_fails() {
    // The delete is what gives the vmm_drv hold back. A sweep that
    // stopped at the first refused reset would leave VM_DESTROY_SELF
    // waiting for that hold in an untimed cv_wait.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingReset]);

    halt_link(&link, &null_log());

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
}

#[test]
fn a_ring_state_read_the_kernel_refused_fails_the_export() {
    // The indices are the kernel's alone: viona never moves the pair
    // userspace keeps, so nothing stands in for them. A pair invented
    // here would resume the destination from a point the ring passed.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingGetState]);
    let device = device_with(Arc::new(link));

    device
        .kernel_ring_state(1)
        .expect_err("the kernel refused the read");
    VirtioDevice::kernel_ring_indices(&device, 1)
        .expect_err("a refused read must fail the export");
}

#[test]
fn a_promiscuous_mode_the_kernel_refused_reaches_the_caller() {
    // The zone asked for every frame the physical link carries. A
    // device that reported success would leave a guest with
    // allow_mac_spoofing silently deaf.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetPromisc]);
    let device = device_with(Arc::new(link));

    device
        .set_promisc(true)
        .expect_err("the kernel refused the mode");
}

#[test]
fn a_notify_port_the_kernel_refused_does_not_stop_the_window() {
    // The two are independent, and the memory window is the one a
    // VirtIO 1.0 guest kicks through. Stopping at the first refusal
    // would cost the guest the in-kernel kick on the route that still
    // works.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetNotifyIop]);
    let device = device_with(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, 0x2000, 0xC000_0000);

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetNotifyIop(0x2000),
            Step::SetNotifyMmio {
                addr: 0xC000_0000,
                size: 0x1000,
            },
        ],
    );
}

#[test]
fn a_deferred_start_kicks_every_ring_though_a_kick_fails() {
    // One ring the kernel refuses to kick must not cost the other its
    // kick. On a migrated guest the unkicked one delivers nothing
    // until the guest kicks it itself, which on an idle rx ring never
    // happens.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingKick]);
    let device = running_device(Arc::new(link));
    device.defer_intr_poll(null_log());

    device.start_poll_deferred();
    // The halt joins the poll thread this started, so the test never
    // leaves one behind.
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .take_while(|step| **step != Step::RingReset(0))
            .copied()
            .collect::<Vec<_>>(),
        [Step::RingKick(0), Step::RingKick(1)],
    );
}

#[test]
fn a_status_the_kernel_refused_raises_nothing() {
    // The status is how the poll thread learns which ring the kernel
    // signalled. Without it there is nothing to deliver: a raise made
    // anyway would put an interrupt on a ring with no completion, and
    // on INTx that is a level the driver's handler cannot clear.
    let steps = Steps::default();
    let link = TestLink::new(&steps).pending(0).failing(&[Op::IntrStatus]);
    let (slot, raised) = wired_interrupt();

    deliver_pending(&link, &slot, &null_log());

    assert!(
        raised.lock().expect("raised lock").is_empty(),
        "a wakeup with no status raised an interrupt",
    );
    assert_eq!(
        steps_of(&steps),
        [],
        "a wakeup with no status cleared a ring",
    );
}

#[test]
fn an_interrupt_the_kernel_would_not_clear_is_still_raised() {
    // The clear and the raise answer to different things: the clear
    // stops viona signalling the same interrupt again, the raise is
    // the completion the guest is waiting for. A refused clear costs a
    // spin. Dropping the raise with it would cost the guest its I/O.
    let steps = Steps::default();
    let link = TestLink::new(&steps).pending(0).failing(&[Op::RingIntrClr]);
    let (slot, raised) = wired_interrupt();

    deliver_pending(&link, &slot, &null_log());

    assert_eq!(cleared(&steps), [0], "the clear was never tried");
    assert_eq!(
        *raised.lock().expect("raised lock"),
        [0],
        "a refused clear dropped the guest's interrupt",
    );
}

#[test]
fn a_wait_the_kernel_refused_ends_the_poll_thread() {
    // The wait is the only thing that parks this thread. An error it
    // waited through would be an error it meets again at once: a tight
    // loop on one core, and a halt that joins a thread which never
    // leaves.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::WaitIntr]);
    let (slot, raised) = wired_interrupt();
    let (stop, _wake) = std::io::pipe().expect("the test can open a pipe");

    // On its own thread, so a loop that spins is a failure here and
    // not a run that never ends.
    let (done, left) = mpsc::channel();
    std::thread::Builder::new()
        .name("poll-loop".into())
        .spawn(move || {
            viona_intr_poll_loop(&link, stop, &slot, null_log());
            done.send(()).expect("the test waits for this");
        })
        .expect("the test can spawn a thread");

    left.recv_timeout(WEDGED)
        .expect("the poll thread left after the wait failed");
    assert!(
        raised.lock().expect("raised lock").is_empty(),
        "a failed wait delivered an interrupt",
    );
}
