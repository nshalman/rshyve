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

use std::sync::{Arc, Mutex};

use vmm_devices::Lifecycle;

use super::device::{addressed_queue, device_with, running_device};
use super::{cleared, null_log, steps_of, Op, Step, Steps, TestLink};
use crate::pci::intr::BackendIntr;
use crate::viona::halt::halt_link;
use crate::viona::poll::deliver_pending;
use crate::viona::{
    RingState, VionaInterrupt, NET_NUM_QUEUES, NET_QUEUE_SIZE, VIRTIO_NET_F_MAC,
};
use crate::VirtioDevice;

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
fn wired_interrupt() -> (Mutex<Option<VionaInterrupt>>, Arc<Mutex<Vec<u16>>>) {
    let raised = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&raised);
    let intr = BackendIntr::detached(move |_session, ring| {
        sink.lock().expect("raised lock").push(ring);
    });
    (Mutex::new(Some(intr)), raised)
}

#[test]
fn a_ring_the_kernel_refused_to_program_is_not_ready() {
    // The ready flag is what lets the next kick through. A ring the
    // kernel refused has no worker: the kick is refused at best, and
    // on an older viona it dereferences a NULL in the mac layer.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingInit]);
    let device = device_with(Arc::new(link));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init; NET_NUM_QUEUES],
        "a ring the kernel refused was marked ready",
    );
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
fn a_ring_state_read_the_kernel_refused_reports_no_progress() {
    // These two indices go into the migration payload, and the read
    // cannot report to its caller. A refusal has to look like a ring
    // that moved nothing, not like whatever the last ring answered.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingGetState]);
    let device = device_with(Arc::new(link));

    assert_eq!(VirtioDevice::get_kernel_ring_indices(&device, 1), (0, 0));
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
