// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The VM's ring gate: every ring worker gone before a write-lock
//! ioctl, nothing that can start one until the gate lets go, and the
//! indices an export reads are the ones the gate saw.

use std::sync::Arc;

use viona_api::vioc_ring_state;
use vmm_core::rings::RingGate;

use super::device::{
    addressed_queue, device_in, device_with, notify, running_device,
};
use super::{steps_of, Op, Step, Steps, TestLink};
use crate::viona::{RingState, NET_QUEUE_SIZE, VIRTIO_NET_F_MAC};
use crate::VirtioDevice;

const PORT: u16 = 0x2010;
const WINDOW: u64 = 0xC000_2000;

/// What the test link reports for a ring it runs.
fn saved(ring: u16) -> vioc_ring_state {
    vioc_ring_state {
        vrs_index: ring,
        vrs_avail_idx: 0x1111 + ring,
        vrs_used_idx: 0x2222 + ring,
        vrs_qsize: NET_QUEUE_SIZE,
        ..Default::default()
    }
}

fn quiesce_steps() -> Vec<Step> {
    vec![
        Step::RingPause(0),
        Step::RingGetState(0),
        Step::RingReset(0),
        Step::RingPause(1),
        Step::RingGetState(1),
        Step::RingReset(1),
    ]
}

fn restart_steps() -> Vec<Step> {
    vec![
        Step::RingSetState(saved(0)),
        Step::RingSetState(saved(1)),
        Step::RingKick(0),
        Step::RingKick(1),
    ]
}

fn clear(steps: &Steps) {
    steps.lock().expect("steps lock").clear();
}

#[test]
fn a_write_lock_call_runs_with_every_worker_gone_and_restarts_them() {
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));

    let seen = Arc::clone(&steps);
    device
        .rings
        .run(|_| {
            assert_eq!(steps_of(&seen), quiesce_steps(), "a worker lived");
        })
        .expect("the rings stop");

    let mut want = quiesce_steps();
    want.extend(restart_steps());
    assert_eq!(steps_of(&steps), want);
}

#[test]
fn a_ring_that_will_not_reset_refuses_the_call_and_is_quarantined() {
    // Its worker may be alive, so the write lock must not be taken.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps)
            .live_rings(NET_QUEUE_SIZE)
            .failing(&[Op::RingReset]),
    ));

    let ran = device.rings.run(|_| panic!("ran with a worker alive"));

    assert!(ran.is_err());
    assert!(!device.rings.is_held());
    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.ring_state[0], RingState::Error);
    assert!(inner.stopped.is_none(), "a refused stop left a proof");
}

#[test]
fn an_export_reads_the_indices_the_hold_saw_and_never_asks_again() {
    // The ring is reset under the hold, so the kernel has nothing left
    // to read. The payload carries what the gate read when it stopped
    // the ring, and the destination resumes from exactly that.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));
    let hold = device.rings.hold().expect("the rings stop");
    assert_eq!(steps_of(&steps), quiesce_steps());
    clear(&steps);

    VirtioDevice::pause_rings_for_export(&device).expect("held stopped");
    for ring in 0..2u16 {
        assert_eq!(
            VirtioDevice::kernel_ring_indices(&device, ring).expect("reads"),
            Some((0x1111 + ring, 0x2222 + ring)),
        );
    }
    assert_eq!(steps_of(&steps), [], "the export reached the kernel");

    VirtioDevice::resume_rings_after_migration(&device).expect("rollback");
    assert_eq!(steps_of(&steps), [], "a ring restarted before VM_RESUME");

    drop(hold);
    assert_eq!(steps_of(&steps), restart_steps());
}

#[test]
fn an_export_without_the_hold_is_refused() {
    // A pause alone leaves the worker alive, and the VM pause that
    // should have held the rings took the write lock beside it.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));

    VirtioDevice::pause_rings_for_export(&device)
        .expect_err("rings that run must fail the export");
    assert_eq!(steps_of(&steps), []);
}

#[test]
fn a_guest_kick_under_a_hold_starts_no_ring() {
    // A paused ring goes back to its init state, and a kick there
    // starts it. Under the hold the ring is reset, and the device
    // sends no kick at all.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));
    let hold = device.rings.hold().expect("the rings stop");
    clear(&steps);

    notify(&device, 0);
    notify(&device, 1);
    device.start_poll_deferred();

    assert_eq!(steps_of(&steps), []);
    drop(hold);
}

#[test]
fn a_ring_the_guest_programs_under_a_hold_starts_with_the_gate() {
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));
    let hold = device.rings.hold().expect("the rings stop");
    clear(&steps);

    VirtioDevice::queue_addr_set(&device, 0, &addressed_queue());
    VirtioDevice::reset(&device);
    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());
    assert_eq!(steps_of(&steps), [], "a ring was programmed under the hold");

    // The reset took the features too, so only the state goes back.
    drop(hold);
    assert_eq!(
        steps_of(&steps),
        [Step::RingSetState(vioc_ring_state {
            vrs_index: 1,
            vrs_qsize: NET_QUEUE_SIZE,
            vrs_qaddr_desc: 0x1_0000,
            vrs_qaddr_avail: 0x2_0000,
            vrs_qaddr_used: 0x3_0000,
            ..Default::default()
        })],
    );
}

#[test]
fn a_restore_under_a_hold_is_checked_then_stopped_until_the_gate_lets_go() {
    // The destination's VM pause holds the rings. The kernel still
    // checks the ring before the commit, and its worker is gone again
    // before any write-lock ioctl of the import.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));
    let hold = device.rings.hold().expect("the rings stop");
    VirtioDevice::reset_all_rings(&device).expect("nothing to stop");
    clear(&steps);

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0xFEE0_0000,
        0x21,
    )
    .expect("a sane payload restores");

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC)),
            Step::RingSetState(vioc_ring_state {
                vrs_index: 1,
                vrs_avail_idx: 7,
                vrs_used_idx: 5,
                vrs_qsize: NET_QUEUE_SIZE,
                vrs_qaddr_desc: 0x1_0000,
                vrs_qaddr_avail: 0x2_0000,
                vrs_qaddr_used: 0x3_0000,
            }),
            Step::RingSetMsi {
                ring: 1,
                addr: 0xFEE0_0000,
                msg: 0x21,
            },
            Step::RingPause(1),
            Step::RingGetState(1),
            Step::RingReset(1),
        ],
    );
    clear(&steps);

    drop(hold);
    assert_eq!(
        steps_of(&steps),
        [
            Step::RingSetState(saved(1)),
            Step::RingSetMsi {
                ring: 1,
                addr: 0xFEE0_0000,
                msg: 0x21,
            },
            Step::RingKick(1),
        ],
    );
}

#[test]
fn a_restored_ring_that_will_not_stop_fails_the_import() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    let hold = device.rings.hold().expect("the rings stop");
    VirtioDevice::reset_all_rings(&device).expect("nothing to stop");
    let link_fails = Arc::new(TestLink::new(&steps).failing(&[Op::RingReset]));
    device.inner.lock().expect("viona lock").link = link_fails;

    VirtioDevice::restore_ring_state(
        &device,
        0,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0,
        0,
    )
    .expect_err("a worker alive under the hold must fail the import");
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[0],
        RingState::Error,
    );
    // Under the hold a run stops nothing, so it would take the write
    // lock beside that worker.
    assert_eq!(
        device.rings.run(|_| ()).err(),
        Some(vmm_core::rings::GateError::Tainted),
    );
    drop(hold);
}

#[test]
fn a_restored_ring_that_will_not_pause_is_still_reset() {
    // Only the reset ends the worker, so it is tried whatever the
    // pause did. The state is lost, and the gate stays usable.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    let hold = device.rings.hold().expect("the rings stop");
    VirtioDevice::reset_all_rings(&device).expect("nothing to stop");
    let link = Arc::new(TestLink::new(&steps).failing(&[Op::RingPause]));
    device.inner.lock().expect("viona lock").link = link;
    clear(&steps);

    restore_result(&device, 0)
        .expect_err("a ring whose state is lost fails the import");

    assert_eq!(steps_of(&steps).last(), Some(&Step::RingReset(0)));
    device.rings.run(|_| ()).expect("the gate is not tainted");
    drop(hold);
}

#[test]
fn a_halt_with_a_hook_waits_for_a_neighbour_to_stop() {
    // A neighbour in the middle of its own delete refuses its ring
    // calls for a moment. The hook comes off only once every ring of
    // the VM is stopped.
    let gate = RingGate::new();
    let mine = Steps::default();
    let other = Steps::default();
    let device = device_in(
        Arc::new(TestLink::new(&mine).live_rings(NET_QUEUE_SIZE)),
        Arc::clone(&gate),
    );
    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);
    let _neighbour = device_in(
        Arc::new(
            TestLink::new(&other)
                .live_rings(NET_QUEUE_SIZE)
                .interrupted_on(Op::RingPause, 9),
        ),
        Arc::clone(&gate),
    );
    clear(&mine);

    vmm_devices::Lifecycle::halt(&device);

    let seen = steps_of(&mine);
    assert!(seen.contains(&Step::SetNotifyIop(0)), "{seen:?}");
    assert_eq!(seen.last(), Some(&Step::Delete));
    let theirs = steps_of(&other);
    let paused = theirs.iter().filter(|s| **s == Step::RingPause(0)).count();
    // One quiesce tries a pause eight times, so a ninth means the halt
    // came back for a second one.
    assert!(paused > 8, "the halt never tried again: {theirs:?}");
    assert!(theirs.contains(&Step::RingReset(1)), "{theirs:?}");
}

#[test]
fn a_hook_change_stops_the_rings_of_every_link_on_the_vm() {
    // The write lock expires every lease on the VM, not only this
    // link's.
    let gate = RingGate::new();
    let mine = Steps::default();
    let other = Steps::default();
    let device = device_in(Arc::new(TestLink::new(&mine)), Arc::clone(&gate));
    let neighbour = device_in(
        Arc::new(TestLink::new(&other).live_rings(NET_QUEUE_SIZE)),
        Arc::clone(&gate),
    );
    {
        let mut inner = neighbour.inner.lock().expect("viona lock");
        inner.negotiated_features = u64::from(VIRTIO_NET_F_MAC);
        inner.ring_state = [RingState::Ready; 2];
    }

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    let mut want = quiesce_steps();
    want.extend(restart_steps());
    assert_eq!(steps_of(&other), want);
}

#[test]
fn a_hook_change_under_a_hold_stops_nothing_again() {
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));
    let hold = device.rings.hold().expect("the rings stop");
    clear(&steps);

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetNotifyIop(PORT),
            Step::SetNotifyMmio {
                addr: WINDOW,
                size: 0x1000
            },
        ],
    );
    drop(hold);
}

#[test]
fn a_halt_takes_the_hook_off_inside_the_gate_before_the_delete() {
    // The delete removes a hook under the VM write lock before it
    // resets any ring.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).live_rings(NET_QUEUE_SIZE),
    ));
    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);
    clear(&steps);

    vmm_devices::Lifecycle::halt(&device);

    let mut want = quiesce_steps();
    want.extend([
        Step::SetNotifyIop(0),
        Step::SetNotifyMmio {
            addr: 0,
            size: 0x1000,
        },
        Step::RingReset(0),
        Step::RingReset(1),
        Step::Delete,
    ]);
    assert_eq!(steps_of(&steps), want, "a deleted link was written back");
}

#[test]
fn a_deleted_link_does_not_block_the_gate() {
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    vmm_devices::Lifecycle::halt(&device);
    clear(&steps);

    device.rings.run(|_| ()).expect("nothing left to stop");

    assert_eq!(steps_of(&steps), []);
}

#[test]
fn a_migration_carries_the_indices_the_source_hold_saw() {
    // Source: rings running at known indices, then the VM pause holds
    // them and the export reads them.
    let indices = [(300u16, 290u16), (7, 5)];
    let src_steps = Steps::default();
    let source =
        running_device(Arc::new(TestLink::new(&src_steps).remembering()));
    for (ring, &(avail, used)) in (0u16..).zip(&indices) {
        restore(&source, ring, avail, used);
    }
    let src_hold = source.rings.hold().expect("the source rings stop");
    let payload: Vec<(u16, u16)> = (0..2u16)
        .map(|ring| {
            VirtioDevice::kernel_ring_indices(&source, ring)
                .expect("reads")
                .expect("viona owns the cursors")
        })
        .collect();
    assert_eq!(payload, indices);

    // Destination: restored under its own VM pause, started after it.
    let dst_steps = Steps::default();
    let dest =
        running_device(Arc::new(TestLink::new(&dst_steps).remembering()));
    let dst_hold = dest.rings.hold().expect("the destination rings stop");
    VirtioDevice::reset_all_rings(&dest).expect("nothing to stop");
    for (ring, &(avail, used)) in (0u16..).zip(&payload) {
        restore(&dest, ring, avail, used);
    }
    clear(&dst_steps);
    drop(dst_hold);

    let started: Vec<(u16, u16)> = steps_of(&dst_steps)
        .iter()
        .filter_map(|s| match s {
            Step::RingSetState(st) => Some((st.vrs_avail_idx, st.vrs_used_idx)),
            _ => None,
        })
        .collect();
    assert_eq!(started, indices);
    drop(src_hold);
}

fn restore_result(
    device: &crate::viona::VirtioViona,
    ring: u16,
) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
    VirtioDevice::restore_ring_state(
        device,
        ring,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0,
        0,
    )
}

fn restore(
    device: &crate::viona::VirtioViona,
    ring: u16,
    avail: u16,
    used: u16,
) {
    VirtioDevice::restore_ring_state(
        device,
        ring,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        avail,
        used,
        0,
        0,
    )
    .expect("restores");
}
