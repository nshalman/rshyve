// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The async storage interface, driven by a backend that finishes
//! requests from a thread of its own.

use super::*;
use crate::nvme::backend::RequestId;

#[test]
fn an_async_backend_completes_from_its_own_thread() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    h.create_io_queues(2, IO_QDEPTH, SQ2, CQ2);

    h.fill(0x50000, 2 * 512, 0x5A);
    let (mut t1, mut t2) = (0, 0);
    submit_on(&h, 1, SQ1, &mut t1, Sqe::write(40, 2, 0x50000, 0), 0x11);
    assert_eq!(manual.queued(), 1, "the write reached the backend");
    manual.complete_all();
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!((cqe.cid, cqe.sqid), (0x11, 1));
    assert_eq!(cqe.status(), Status::SUCCESS);

    submit_on(&h, 2, SQ2, &mut t2, Sqe::read(40, 2, 0x60000, 0), 0x22);
    manual.complete_all();
    let cqe = h.wait_cqe(CQ2, 0, true);
    assert_eq!((cqe.cid, cqe.sqid), (0x22, 2));
    assert_eq!(cqe.status(), Status::SUCCESS);

    assert_eq!(h.read_guest(0x60000, 2 * 512), vec![0x5A; 2 * 512]);
    assert_eq!(manual.submitted_queues(), vec![1, 2]);
}

#[test]
fn a_queue_admits_no_more_than_the_backend_depth() {
    let manual = Manual::new(2);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    for slot in 0..4u16 {
        put_sqe(
            &h,
            SQ1,
            slot,
            Sqe::read(u64::from(slot), 1, 0x50000, 0),
            slot,
        );
    }
    h.sq_doorbell(1, 4);
    assert_eq!(manual.queued(), 2, "admitted past the backend depth");

    // Room frees as completions arrive, and the commands left in the
    // guest's queue go out without another doorbell.
    manual.complete_all();
    assert_eq!(manual.queued(), 2, "the rest did not follow");
    manual.complete_all();
    for slot in 0..4u16 {
        let cqe = h.wait_cqe(CQ1, slot, true);
        assert_eq!(cqe.status(), Status::SUCCESS);
    }
    assert_eq!(manual.queued(), 0);
}

#[test]
fn a_repeated_or_unknown_completion_is_ignored() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x31);
    let req = manual.take().pop().expect("one request");
    let id = req.id();
    manual.sink().complete(&[(id, BlockStatus::Success)]);
    assert_eq!(h.wait_cqe(CQ1, 0, true).cid, 0x31);

    let stale = RequestId {
        serial: id.serial + 7,
        ..id
    };
    let nowhere = RequestId { queue: 99, ..id };
    manual.sink().complete(&[
        (id, BlockStatus::Success),
        (stale, BlockStatus::Success),
        (nowhere, BlockStatus::Success),
    ]);
    assert_eq!(
        h.read_guest(CQ1 + CQE_SIZE as u64, CQE_SIZE),
        vec![0u8; CQE_SIZE],
        "a completion that names no command was posted"
    );
}

#[test]
fn a_backend_status_reaches_the_guest_as_nvme_status() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let cases = [
        (
            Sqe::read(0, 1, 0x50000, 0),
            BlockStatus::MediaError,
            SC_UNRECOVERED_READ,
        ),
        (
            Sqe::write(0, 1, 0x50000, 0),
            BlockStatus::MediaError,
            SC_WRITE_FAULT,
        ),
        (
            Sqe::write(0, 1, 0x50000, 0),
            BlockStatus::NoSpace,
            SC_CAPACITY_EXCEEDED | SC_DNR,
        ),
        (
            Sqe::read(0, 1, 0x50000, 0),
            BlockStatus::LbaOutOfRange,
            SC_LBA_OUT_OF_RANGE | SC_DNR,
        ),
    ];
    let mut tail = 0;
    for (slot, (sqe, status, _)) in cases.iter().enumerate() {
        submit_on(&h, 1, SQ1, &mut tail, *sqe, slot as u16);
        let req = manual.take().pop().expect("one request");
        manual.sink().complete(&[(req.id(), *status)]);
    }
    for (slot, (_, _, code)) in cases.iter().enumerate() {
        let cqe = h.wait_cqe(CQ1, slot as u16, true);
        assert_eq!(cqe.status(), Status::of(*code), "case {slot}");
    }
}

/// After a Delete I/O SQ the driver owns the pages its commands named,
/// so a backend that gets to one late must not write them.
#[test]
fn a_deleted_queue_takes_its_pages_back_from_the_backend() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.fill(0x50000, PAGE_SIZE, 0xAA);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 8, 0x50000, 0), 0x41);
    let req = manual.take().pop().expect("one request");

    // The delete waits for the command, so the backend finishes it from
    // another thread once the drain has started.
    let ctrl = Arc::clone(&h.ctrl);
    let sink = Arc::clone(manual.sink());
    let finisher = std::thread::spawn(move || {
        let start = Instant::now();
        while ctrl.drain_count() == 0 {
            assert!(start.elapsed() < WAIT, "the delete never drained");
            std::thread::sleep(Duration::from_millis(1));
        }
        let live = req.data().is_live();
        let copied = req.data().copy_to_guest(&[0x77; 8 * 512]);
        sink.complete(&[(req.id(), BlockStatus::Success)]);
        (live, copied.is_ok())
    });
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    });
    let (live, copied) = finisher.join().expect("finisher");

    assert_eq!(cqe.status(), Status::SUCCESS);
    assert!(!live, "the request still looked live");
    assert!(!copied, "the backend wrote pages the driver took back");
    assert_eq!(h.read_guest(0x50000, PAGE_SIZE), vec![0xAA; PAGE_SIZE]);
    assert_eq!(h.read_guest(CQ1, CQE_SIZE), vec![0u8; CQE_SIZE]);
}

/// A ring backend may refuse a request at once, calling the sink from
/// inside `submit` while the vCPU thread holds the controller lock.
#[test]
fn a_backend_may_complete_inside_submit() {
    let manual = Manual::new(IO_QDEPTH);
    manual.inline.store(true, Ordering::Relaxed);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    for slot in 0..2u16 {
        put_sqe(
            &h,
            SQ1,
            slot,
            Sqe::read(u64::from(slot), 1, 0x50000, 0),
            slot,
        );
    }
    h.sq_doorbell(1, 2);

    for slot in 0..2u16 {
        let cqe = h.wait_cqe(CQ1, slot, true);
        assert_eq!((cqe.cid, cqe.status()), (slot, Status::SUCCESS));
    }
    assert_eq!(manual.queued(), 0);
    let expect: Vec<u8> = (512..1024).map(backing_byte).collect();
    assert_eq!(h.read_guest(0x50000, 512), expect);
}
