// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Completions that find the guest's completion queue full.

use super::*;

/// Every completion in the CQ at `base` from `slots`, as (cid, phase).
fn cqes(h: &Harness, base: u64, slots: &[u16]) -> Vec<(u16, bool)> {
    slots
        .iter()
        .map(|&slot| {
            let at = base + u64::from(slot) * CQE_SIZE as u64;
            let cqe = Cqe::decode(&h.read_guest(at, CQE_SIZE));
            (cqe.cid, cqe.phase)
        })
        .collect()
}

#[test]
fn a_full_cq_holds_completions_until_the_guest_makes_room() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    // Four entries: three usable, since a full ring is tail + 1 == head.
    h.create_io_queues_sized(1, IO_QDEPTH, 4, SQ1, CQ1);

    let mut tail = 0;
    for cid in 0..6u16 {
        submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    manual.complete_all();
    assert_eq!(
        cqes(&h, CQ1, &[0, 1, 2]),
        vec![(0, true), (1, true), (2, true)]
    );
    assert_eq!(
        h.read_guest(CQ1 + 3 * CQE_SIZE as u64, CQE_SIZE),
        vec![0; 16]
    );

    // The guest consumed all three; the held ones go in behind them,
    // wrapping with the phase flipped.
    h.cq_doorbell(1, 3);
    assert_eq!(
        cqes(&h, CQ1, &[3, 0, 1]),
        vec![(3, true), (4, false), (5, false)]
    );
}

/// A held completion keeps its command's tag, so a guest that never
/// frees CQ space cannot make the controller hold more than the depth.
#[test]
fn held_completions_keep_their_tags() {
    let manual = Manual::new(2);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues_sized(1, IO_QDEPTH, 2, SQ1, CQ1);

    let mut tail = 0;
    for cid in 0..3u16 {
        submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    assert_eq!(manual.queued(), 2);
    // One completion fits; its tag lets the third command in.
    manual.complete_all();
    assert_eq!(manual.queued(), 1);
    manual.complete_all();

    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 3);
    assert_eq!(manual.queued(), 0, "admitted past held completions");

    h.cq_doorbell(1, 1);
    assert_eq!(cqes(&h, CQ1, &[1]), vec![(1, true)]);
    assert_eq!(manual.queued(), 1, "a written completion freed no tag");
}

#[test]
fn the_admin_queue_waits_in_the_sq_while_its_cq_is_full() {
    let mut h = Harness::new();
    h.enable();
    let get = Sqe {
        opcode: ADMIN_OPC_GET_FEATURES,
        cdw10: u32::from(FEAT_NUM_QUEUES),
        ..Sqe::default()
    };
    // Fill the admin CQ: sixteen entries hold fifteen.
    for slot in 0..ADMIN_QDEPTH - 1 {
        put_sqe(&h, ASQ, slot, get, slot + 1);
    }
    h.sq_doorbell(ADMIN_QUEUE_ID, ADMIN_QDEPTH - 1);
    put_sqe(&h, ASQ, ADMIN_QDEPTH - 1, get, ADMIN_QDEPTH);
    h.sq_doorbell(ADMIN_QUEUE_ID, 0);

    let last = ACQ + u64::from(ADMIN_QDEPTH - 1) * CQE_SIZE as u64;
    assert_eq!(h.read_guest(last, CQE_SIZE), vec![0; 16], "CQ overrun");
    let head = h.ctrl.lock().admin_sq.as_ref().expect("admin SQ").head;
    assert_eq!(head, ADMIN_QDEPTH - 1, "ran a command with no CQ room");

    h.cq_doorbell(ADMIN_QUEUE_ID, ADMIN_QDEPTH - 1);
    let cqe = Cqe::decode(&h.read_guest(last, CQE_SIZE));
    assert_eq!((cqe.cid, cqe.phase), (ADMIN_QDEPTH, true));
    assert_eq!(cqe.status(), Status::SUCCESS);
}

/// A Delete I/O SQ aborts its commands, the held ones included, and
/// their tags come back for the queue that replaces it.
#[test]
fn deleting_an_sq_drops_its_held_completions() {
    let manual = Manual::new(2);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues_sized(1, IO_QDEPTH, 2, SQ1, CQ1);

    let mut tail = 0;
    for cid in 0..2u16 {
        submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    manual.complete_all();
    assert_eq!(cqes(&h, CQ1, &[0]), vec![(0, true)]);

    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    h.cq_doorbell(1, 1);
    assert_eq!(h.read_guest(CQ1 + CQE_SIZE as u64, CQE_SIZE), vec![0; 16]);

    h.create_io_sq(1, IO_QDEPTH, SQ1, 1);
    let mut tail = 0;
    for cid in 10..12u16 {
        submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    assert_eq!(manual.queued(), 2, "the deleted queue kept a tag");
}
