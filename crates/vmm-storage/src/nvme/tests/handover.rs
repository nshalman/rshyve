// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A backend that a live migration detaches and attaches.

use std::time::Instant;

use vmm_devices::lifecycle::BackingAttach;

use super::*;

fn now() -> BackingAttach {
    BackingAttach {
        until: Instant::now(),
        allow_unproven: false,
    }
}

#[test]
fn a_detached_store_takes_no_command_until_it_attaches() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.ctrl.pause();
    h.ctrl.detach_backing().expect("detach a paused, idle disk");
    assert!(!h.ctrl.backing_attached());

    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 7);
    // A resume alone must not reach a store that is not there.
    h.ctrl.resume();
    assert_eq!(manual.queued(), 0, "a command reached a detached store");

    h.ctrl.attach_backing(&now()).expect("attach");
    h.ctrl.resume();
    assert_eq!(manual.queued(), 1, "the queued command did not go out");
    manual.complete_all();
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!((cqe.cid, cqe.status()), (7, Status::SUCCESS));
}

#[test]
fn the_store_changes_hands_only_paused_and_idle() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let why = h.ctrl.detach_backing().expect_err("running");
    assert!(why.to_string().contains("paused"), "{why}");
    let why = h.ctrl.attach_backing(&now()).expect_err("running");
    assert!(why.to_string().contains("paused"), "{why}");

    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 1);
    assert_eq!(manual.queued(), 1);
    h.ctrl.pause();
    let why = h.ctrl.detach_backing().expect_err("in flight");
    assert!(why.to_string().contains("in flight"), "{why}");
    assert!(h.ctrl.backing_attached());

    manual.complete_all();
    h.ctrl.detach_backing().expect("idle now");
}

#[test]
fn a_pause_drains_the_ring_into_the_guest_queue_before_the_handover() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 3);
    h.ctrl.pause();
    // Queued after the barrier closed: stays in the guest SQ.
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(1, 1, 0x51000, 0), 4);
    assert!(!h.ctrl.is_quiesced(), "quiesced with a request in the ring");
    assert_eq!(manual.queued(), 1, "a command passed the closed barrier");

    manual.complete_all();
    assert!(h.wait_quiesced(Duration::from_secs(2)));
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!((cqe.cid, cqe.status()), (3, Status::SUCCESS));

    h.ctrl.detach_backing().expect("drained");
    h.ctrl.attach_backing(&now()).expect("attach");
    h.ctrl.resume();
    // The command the guest left queued goes out once, after the attach.
    assert_eq!(manual.queued(), 1);
    manual.complete_all();
    let cqe = h.wait_cqe(CQ1, 1, true);
    assert_eq!(cqe.cid, 4);
}
