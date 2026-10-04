// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The admission barrier, and the drains that wait behind it.

use vmm_devices::lifecycle::NvmeMigrateState;

use super::*;

fn nvme_state(h: &Harness) -> NvmeMigrateState {
    match h.ctrl.export_migrate_state() {
        Ok(Some(vmm_devices::DeviceMigrateState::Nvme(state))) => state,
        other => panic!("no NVMe state: {other:?}"),
    }
}

#[test]
fn a_paused_controller_leaves_commands_in_the_guest_queue() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.ctrl.pause();
    for slot in 0..2u16 {
        put_sqe(&h, SQ1, slot, Sqe::read(0, 1, 0x50000, 0), slot);
    }
    h.sq_doorbell(1, 2);

    assert_eq!(manual.queued(), 0, "a paused controller admitted I/O");
    let sq = nvme_state(&h).io_sqs[0].clone().expect("SQ 1");
    assert_eq!((sq.head, sq.tail), (0, 2), "the head moved while paused");
    assert!(h.ctrl.is_quiesced());

    // No doorbell follows: the guest rang it once, while paused.
    h.ctrl.resume();
    assert_eq!(manual.queued(), 2, "resume left the commands queued");
    manual.complete_all();
    assert_eq!(h.wait_cqe(CQ1, 0, true).cid, 0);
    assert_eq!(h.wait_cqe(CQ1, 1, true).cid, 1);
}

/// Steady doorbells on one thread, a pause on another: once the
/// controller reports quiesced, nothing more may reach the backend.
#[test]
fn nothing_is_admitted_after_a_pause_drains() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let stop = Arc::new(AtomicBool::new(false));
    let submitter = {
        let ctrl = Arc::clone(&h.ctrl);
        let mem = h.mem();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut tail = 0u16;
            while !stop.load(Ordering::Relaxed) {
                let sqe = Sqe::read(0, 1, 0x70000, 0);
                mem.write(
                    SQ1 + u64::from(tail) * SQE_SIZE as u64,
                    &sqe.encode(tail),
                )
                .expect("write SQE");
                tail = (tail + 1) % IO_QDEPTH;
                let wo = WriteOp::from_buf(&u32::from(tail).to_le_bytes());
                ctrl.bar_rw(
                    BarN::BAR0,
                    REG_DOORBELL_BASE + 8,
                    RWOp::Write(&wo),
                );
            }
        })
    };
    let start = Instant::now();
    while backend.started() < 32 {
        assert!(start.elapsed() < WAIT, "the submitter never got going");
        std::thread::yield_now();
    }

    h.ctrl.pause();
    assert!(h.wait_quiesced(WAIT), "the pause did not drain");
    let at_quiesce = backend.started();
    std::thread::sleep(Duration::from_millis(50));
    let after = backend.started();

    stop.store(true, Ordering::Relaxed);
    submitter.join().expect("submitter");
    assert_eq!(after, at_quiesce, "I/O slipped past a closed barrier");
    assert!(h.ctrl.is_quiesced());
}

/// A command a worker has not picked up yet is still the guest's
/// command, and the reset has to wait for it as well.
#[test]
fn a_reset_waits_for_commands_still_queued_for_a_worker() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let total = crate::nvme::NUM_WORKERS + 4;
    let mut tail = 0;
    for cid in 0..total as u16 {
        io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    backend.wait_in_flight(crate::nvme::NUM_WORKERS);

    let releaser = {
        let ctrl = Arc::clone(&h.ctrl);
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || {
            while ctrl.drain_count() == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            backend.release();
        })
    };
    h.reg_write32(REG_CC, 0);
    releaser.join().expect("releaser");

    assert_eq!(h.csts() & CSTS_RDY, 0);
    assert_eq!(
        h.ctrl.counts.in_flight.load(Ordering::Acquire),
        0,
        "RDY cleared with commands in flight"
    );
    // The commands still in the channel never reached the store.
    assert_eq!(backend.started(), crate::nvme::NUM_WORKERS);
}

#[test]
fn export_waits_for_the_command_in_flight() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x51);
    backend.wait_in_flight(1);
    h.ctrl.pause();

    let exporter = {
        let ctrl = Arc::clone(&h.ctrl);
        std::thread::spawn(move || ctrl.export_migrate_state())
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!exporter.is_finished(), "export ran over a live command");
    backend.release();
    let state = match join_within(exporter, WAIT) {
        Ok(Some(vmm_devices::DeviceMigrateState::Nvme(state))) => state,
        other => panic!("no NVMe state: {other:?}"),
    };

    // The completion is in the guest's ring, so the destination has
    // nothing to re-run.
    let cq = state.io_cqs[0].clone().expect("CQ 1");
    assert_eq!(cq.tail, 1);
    assert_eq!(h.wait_cqe(CQ1, 0, true).cid, 0x51);
}

#[test]
fn export_refuses_while_a_command_never_finishes() {
    let (mut h, backend) = Harness::impatient();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x52);
    backend.wait_in_flight(1);
    h.ctrl.pause();

    let error = h.ctrl.export_migrate_state().expect_err("must refuse");
    assert!(error.to_string().contains("in flight"), "{error}");
    backend.release();
}

#[test]
fn delete_sq_completes_only_once_its_commands_have() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 8, 0x50000, 0), 0x53);
    backend.wait_in_flight(1);

    let releaser = {
        let ctrl = Arc::clone(&h.ctrl);
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || {
            while ctrl.drain_count() == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            backend.release();
        })
    };
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    });
    let quiet = backend.in_flight();
    releaser.join().expect("releaser");

    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(quiet, 0, "Delete SQ completed over a live transfer");
}

/// The control plane polls the quiesce query without the lock, so it
/// must not see quiet while the thread that finished the last command
/// still holds the lock writing its CQE.
#[test]
fn quiet_is_reported_only_once_the_finishing_lock_is_released() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x61);
    let req = manual.take().pop().expect("one request");
    h.ctrl.pause();
    assert!(!h.ctrl.is_quiesced());

    let mut st = h.ctrl.lock();
    h.ctrl.finish_now(&mut st, &h.mem(), req.id(), SC_SUCCESS);
    assert!(!h.ctrl.is_quiesced(), "quiet with the CQE lock still held");
    drop(st);

    assert!(h.ctrl.is_quiesced());
    assert_eq!(h.wait_cqe(CQ1, 0, true).cid, 0x61);
}

/// The driver may create a new SQ in the slot while the delete drains;
/// its commands must not hold the delete up or fail it.
#[test]
fn a_delete_waits_for_its_own_queue_not_the_one_after_it() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::from_factory(
        manual.factory(),
        false,
        SHORT_FENCES,
        NvmeConfig::default(),
    )
    .expect("controller");
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x01);
    let old = manual.take().pop().expect("old request");

    let t = h.admin_tail;
    let delete = Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    };
    put_sqe(&h, ASQ, t, delete, 0xD1);
    let deleter = {
        let ctrl = Arc::clone(&h.ctrl);
        std::thread::spawn(move || {
            let wo = WriteOp::from_buf(&u32::from(t + 1).to_le_bytes());
            ctrl.bar_rw(BarN::BAR0, REG_DOORBELL_BASE, RWOp::Write(&wo));
        })
    };
    let start = Instant::now();
    while h.ctrl.drain_count() == 0 {
        assert!(start.elapsed() < WAIT, "the delete never drained");
        std::thread::sleep(Duration::from_millis(1));
    }

    let create = Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ2,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    };
    put_sqe(&h, ASQ, t + 1, create, 0xC1);
    h.sq_doorbell(ADMIN_QUEUE_ID, t + 2);
    let mut tail = 0;
    submit_on(&h, 1, SQ2, &mut tail, Sqe::read(0, 1, 0x60000, 0), 0x02);
    assert_eq!(manual.queued(), 1, "the new queue admitted nothing");

    manual.sink().complete(&[(old.id(), BlockStatus::Success)]);
    deleter.join().expect("deleter");
    assert_eq!(h.csts() & CSTS_CFS, 0, "the new queue failed the delete");
}

/// Linux frees the queue's pages on any Delete Queue completion, so one
/// whose commands outlast the deadline gets none.
#[test]
fn a_delete_that_times_out_posts_no_completion() {
    let (mut h, backend) = Harness::impatient();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 8, 0x50000, 0), 0x54);
    backend.wait_in_flight(1);

    let t = h.admin_tail;
    let delete = Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    };
    put_sqe(&h, ASQ, t, delete, 0xD2);
    h.sq_doorbell(ADMIN_QUEUE_ID, t + 1);

    let slot = ACQ + h.admin_cq_head as u64 * CQE_SIZE as u64;
    let cqe = Cqe::decode(&h.read_guest(slot, CQE_SIZE));
    let csts = h.csts();
    // Before any assert: dropping the controller joins the worker.
    backend.release();
    assert_ne!(cqe.phase, h.admin_phase, "completed over a live transfer");
    assert_ne!(csts & CSTS_CFS, 0, "the controller is not fatal");
}

/// A reset while a Delete Queue drains gives the driver a new admin
/// queue, which never sent that command.
#[test]
fn a_delete_completion_stays_out_of_an_admin_queue_made_during_its_drain() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x01);
    let req = manual.take().pop().expect("one request");

    let t = h.admin_tail;
    let delete = Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    };
    put_sqe(&h, ASQ, t, delete, 0xD3);
    let deleter = {
        let ctrl = Arc::clone(&h.ctrl);
        std::thread::spawn(move || {
            let wo = WriteOp::from_buf(&u32::from(t + 1).to_le_bytes());
            ctrl.bar_rw(BarN::BAR0, REG_DOORBELL_BASE, RWOp::Write(&wo));
        })
    };
    let start = Instant::now();
    while h.ctrl.drain_count() == 0 {
        assert!(start.elapsed() < WAIT, "the delete never drained");
        std::thread::sleep(Duration::from_millis(1));
    }

    // What a reset and a new CC.EN leave behind, in one lock hold so
    // the deleter cannot look in between.
    {
        let mut st = h.ctrl.lock();
        h.ctrl.finish_now(&mut st, &h.mem(), req.id(), SC_SUCCESS);
        let instance = st.next_instance();
        st.admin_cq.as_mut().expect("admin CQ").instance = instance;
    }
    join_within(deleter, WAIT);

    let slot = ACQ + h.admin_cq_head as u64 * CQE_SIZE as u64;
    assert_eq!(
        h.read_guest(slot, CQE_SIZE),
        vec![0; CQE_SIZE],
        "the delete completed into the new admin queue"
    );
}
