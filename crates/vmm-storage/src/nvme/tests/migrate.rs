// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration payload validation: what a peer-supplied controller state
//! must satisfy before any of it reaches the live queues.

use super::*;
use vmm_devices::lifecycle::{
    NvmeMigrateCq, NvmeMigrateCqe, NvmeMigrateSq, NvmeMigrateState,
};

use crate::nvme::migrate::check_nvme_state;

struct NvmeQuiesceTestDevice {
    gate: Arc<QuiesceGate>,
}

impl Lifecycle for NvmeQuiesceTestDevice {
    fn type_name(&self) -> &'static str {
        "nvme"
    }

    fn is_quiesced(&self) -> bool {
        self.gate.is_quiesced()
    }
}

/// A controller payload with one admin queue pair and one I/O
/// queue pair, all of it valid.
fn good_payload() -> NvmeMigrateState {
    NvmeMigrateState {
        cc: CC_EN,
        csts: 1,
        aqa: 0x003F_003F,
        asq_base: 0x1000,
        acq_base: 0x2000,
        admin_sq: Some(NvmeMigrateSq {
            base: 0x1000,
            size: 64,
            head: 3,
            tail: 5,
            cq_id: 0,
        }),
        admin_cq: Some(NvmeMigrateCq {
            base: 0x2000,
            size: 64,
            head: 3,
            tail: 5,
            phase: true,
            iv: 0,
            ien: true,
            held: Vec::new(),
        }),
        io_sqs: vec![Some(NvmeMigrateSq {
            base: 0x3000,
            size: 256,
            head: 0,
            tail: 0,
            cq_id: 1,
        })],
        io_cqs: vec![Some(NvmeMigrateCq {
            base: 0x4000,
            size: 256,
            head: 0,
            tail: 0,
            phase: true,
            iv: 1,
            ien: true,
            held: Vec::new(),
        })],
        num_io_queues: 1,
        pci: Default::default(),
        msix: Default::default(),
        namespace: None,
    }
}

#[test]
fn a_sane_controller_payload_is_accepted() {
    check_nvme_state(&good_payload()).expect("a sane payload");
}

#[test]
fn a_queue_size_of_zero_is_refused() {
    // The doorbell handlers take `% size`; zero divides by zero and
    // a release build aborts on it.
    let mut payload = good_payload();
    payload.admin_sq.as_mut().expect("admin sq").size = 0;
    let error = check_nvme_state(&payload).expect_err("size 0");
    assert!(error.to_string().contains("outside 2..="), "{error}");

    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").size = 1;
    check_nvme_state(&payload).expect_err("size 1 is below the minimum");
}

#[test]
fn a_queue_size_past_the_maximum_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").size = MAX_QUEUE_SIZE;
    check_nvme_state(&payload).expect("the maximum itself is fine");
    payload.io_sqs[0].as_mut().expect("io sq").size = u16::MAX;
    check_nvme_state(&payload).expect_err("past the maximum");
}

#[test]
fn a_cursor_outside_the_ring_is_refused() {
    // `while sq.head != sq.tail` would walk guest memory until the
    // cursor wrapped.
    let mut payload = good_payload();
    payload.admin_sq.as_mut().expect("admin sq").head = 64;
    let error = check_nvme_state(&payload).expect_err("head == size");
    assert!(error.to_string().contains("outside a 64-entry"), "{error}");

    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").tail = 999;
    check_nvme_state(&payload).expect_err("tail past the ring");
}

#[test]
fn an_unaligned_or_zero_queue_base_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").base = 0x3001;
    let error = check_nvme_state(&payload).expect_err("unaligned base");
    assert!(error.to_string().contains("page-aligned"), "{error}");

    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").base = 0;
    check_nvme_state(&payload).expect_err("a zero base");
}

#[test]
fn a_submission_queue_whose_completion_queue_is_absent_is_refused() {
    // Completions would be posted through a `None` slot, or into
    // another queue's ring.
    let mut payload = good_payload();
    payload.io_cqs[0] = None;
    let error = check_nvme_state(&payload).expect_err("missing CQ 1");
    assert!(error.to_string().contains("does not create"), "{error}",);

    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").cq_id = 9;
    check_nvme_state(&payload).expect_err("a CQ id past the table");
}

#[test]
fn an_interrupt_vector_the_msix_table_cannot_hold_is_refused() {
    // Create I/O Completion Queue refuses these, and `MsixTable::fire`
    // drops them silently, so the imported queue would never raise.
    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").iv = NVME_MSIX_COUNT - 1;
    check_nvme_state(&payload).expect("the last vector is fine");

    payload.io_cqs[0].as_mut().expect("io cq").iv = NVME_MSIX_COUNT;
    let error = check_nvme_state(&payload).expect_err("iv past the table");
    assert!(error.to_string().contains("interrupt vector"), "{error}");

    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").iv = u16::MAX;
    check_nvme_state(&payload).expect_err("an admin vector past the table");
}

/// Linux gives its poll queues no interrupt, so a migration that
/// restores one as interrupting sends the destination guest a message
/// it never armed a handler for.
#[test]
fn a_polled_completion_queue_stays_polled_across_a_migration() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues_with_irq(1, IO_QDEPTH, SQ1, CQ1, false);
    let state = src
        .ctrl
        .export_migrate_state()
        .expect("export")
        .expect("an enabled controller has state");

    let mut dst = Harness::new();
    dst.ctrl.restore_migrate_state(&state).expect("restore");
    dst.ctrl.resume();

    let before = dst.msi_count();
    let mut tail = 0;
    io_submit(&mut dst, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x31);
    let cqe = dst.wait_cqe(CQ1, 0, true);

    assert_eq!(cqe.cid, 0x31);
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(dst.msi_count(), before, "a polled CQ raised an interrupt");
}

#[test]
fn more_queues_than_the_controller_has_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs = vec![None; MAX_IO_QUEUES + 1];
    let error = check_nvme_state(&payload).expect_err("too many SQs");
    assert!(error.to_string().contains("of each"), "{error}");
}

#[test]
fn an_out_of_range_io_queue_count_is_refused() {
    let mut payload = good_payload();
    payload.num_io_queues = 0;
    check_nvme_state(&payload).expect_err("zero queues");
    payload.num_io_queues = MAX_IO_QUEUES as u16 + 1;
    check_nvme_state(&payload).expect_err("past the maximum");
}

#[test]
fn quiesce_query_is_non_blocking_with_active_worker() {
    let device: Arc<dyn Lifecycle> = Arc::new(NvmeQuiesceTestDevice {
        gate: Arc::new(QuiesceGate::new(1)),
    });

    let probe = Arc::clone(&device);
    assert!(!vmm_devices_testsupport::assert_query_non_blocking(
        move || probe.is_quiesced()
    ));
}

#[test]
fn cap_register_value() {
    // Verify CAP register encoding
    let mqes = MAX_QUEUE_SIZE - 1;
    let cap: u64 = (u64::from(mqes) << CAP_MQES_SHIFT)
        | CAP_CQR
        | (1u64 << CAP_TO_SHIFT)
        | CAP_CSS_NVM;
    // MQES should be in bits 15:0
    assert_eq!((cap & CAP_MQES_MASK) as u16, mqes);
    // CQR bit 16
    assert_ne!(cap & CAP_CQR, 0);
    // CSS NVM bit 37
    assert_ne!(cap & CAP_CSS_NVM, 0);
}

#[test]
fn cqe_phase_bit_encoding() {
    // Phase bit is bit 0 of the status word in CQE
    let status: u16 = SC_SUCCESS;
    let phase = true;
    let encoded = (status << 1) | if phase { 1 } else { 0 };
    assert_eq!(encoded & 1, 1); // phase set
    assert_eq!(encoded >> 1, SC_SUCCESS);
}

#[test]
fn doorbell_index_calculation() {
    // SQ tail doorbell for queue 0 is at offset 0x1000
    let offset = REG_DOORBELL_BASE;
    let db_offset = offset - REG_DOORBELL_BASE;
    let db_index = db_offset / 4;
    let qid = db_index / 2;
    let is_sq = db_index.is_multiple_of(2);
    assert_eq!(qid, 0);
    assert!(is_sq);

    // CQ head doorbell for queue 0 is at offset 0x1004
    let offset = REG_DOORBELL_BASE + 4;
    let db_offset = offset - REG_DOORBELL_BASE;
    let db_index = db_offset / 4;
    let qid = db_index / 2;
    let is_sq = db_index.is_multiple_of(2);
    assert_eq!(qid, 0);
    assert!(!is_sq);

    // SQ tail doorbell for queue 1 is at offset 0x1008
    let offset = REG_DOORBELL_BASE + 8;
    let db_offset = offset - REG_DOORBELL_BASE;
    let db_index = db_offset / 4;
    let qid = db_index / 2;
    let is_sq = db_index.is_multiple_of(2);
    assert_eq!(qid, 1);
    assert!(is_sq);
}

#[test]
fn prp_single_page() {
    // When total_len fits in one page, only PRP1 is used
    // This is a structural test of the logic (can't test without MemCtx)
    let offset_in_page = 0x100usize;
    let _prp1_addr = 0x1000u64 + offset_in_page as u64;
    let first_len = (PAGE_SIZE - offset_in_page).min(512);
    assert_eq!(first_len, 512); // 512 < 3840
}

#[test]
fn status_code_encoding() {
    // DNR flag should be bit 14
    assert_eq!(SC_DNR, 1 << 14);
    // Invalid opcode with DNR
    let status = SC_INVALID_OPCODE | SC_DNR;
    assert_ne!(status & SC_DNR, 0);
    assert_eq!(status & !SC_DNR, SC_INVALID_OPCODE);
}

#[test]
fn create_io_sq_naming_cq_zero_is_rejected() {
    let h = Harness::new();
    let mut state = NvmeState::default();
    let qid = 1u32;
    let qsize = 1u32 << 16;
    let cq_id = 0u32 << 16;

    let status =
        h.ctrl
            .admin_create_io_sq(&mut state, 0x1000, qid | qsize, cq_id);

    // CQ 0 is the admin queue, so no I/O CQ answers to it. NVMe 1.4
    // figure 128 calls that Completion Queue Invalid, not Invalid Field.
    assert_eq!(status, (SC_CQ_INVALID | SC_DNR, 0));
}

#[test]
fn num_queues_max_request_stays_nonzero() {
    let mut state = NvmeState::default();

    let set_result = NvmeController::admin_set_features(
        &mut state,
        u32::from(FEAT_NUM_QUEUES),
        u32::MAX,
    );

    let allocated = (MAX_IO_QUEUES as u32) - 1;
    assert_eq!(set_result, (SC_SUCCESS, allocated | (allocated << 16)));
    assert_eq!(state.num_io_queues, MAX_IO_QUEUES as u16);
    assert_ne!(state.num_io_queues, 0);
    assert_eq!(
        NvmeController::admin_get_features(&state, u32::from(FEAT_NUM_QUEUES),),
        set_result
    );
}

#[test]
fn create_io_queues_reject_max_qsize_without_overflow() {
    let cdw10 = 1 | (u32::from(u16::MAX) << 16);

    let h = Harness::new();

    let mut cq_state = NvmeState::default();
    assert_eq!(
        h.ctrl.admin_create_io_cq(&mut cq_state, 0x1000, cdw10, 0),
        (SC_INVALID_QUEUE_SIZE | SC_DNR, 0)
    );

    let mut sq_state = NvmeState::default();
    assert_eq!(
        h.ctrl
            .admin_create_io_sq(&mut sq_state, 0x1000, cdw10, 1 << 16),
        (SC_INVALID_QUEUE_SIZE | SC_DNR, 0)
    );
}

#[test]
fn sector_overflow_checked_mul() {
    // Verify that checked_mul catches overflow in sector arithmetic.
    // slba near u64::MAX with block_size=512 would overflow.
    let slba: u64 = u64::MAX / 512 + 1;
    let block_size: u32 = 512;
    assert!(slba.checked_mul(u64::from(block_size)).is_none());
}

#[test]
fn sector_bounds_checked_add() {
    // slba + nlb overflow should be caught
    let slba: u64 = u64::MAX;
    let nlb: u16 = 1;
    assert!(slba.checked_add(u64::from(nlb)).is_none());
}

#[test]
fn nlb_overflow_checked_mul() {
    // nlb * block_size overflow should be caught
    let nlb: u16 = u16::MAX;
    let block_size: u32 = u32::MAX;
    assert!((nlb as u64).checked_mul(u64::from(block_size)).is_some());
    // This fits in u64, so the overflow is in byte_offset, not byte_count
    // Verify the realistic case works
    let nlb: u16 = 128;
    let block_size: u32 = 512;
    assert_eq!((nlb as u64).checked_mul(u64::from(block_size)), Some(65536));
}

fn copy_ram(src: &Harness, dst: &Harness, gpa: u64, len: usize) {
    dst.mem()
        .write(gpa, &src.read_guest(gpa, len))
        .expect("copy guest RAM");
}

fn export(h: &Harness) -> vmm_devices::DeviceMigrateState {
    h.ctrl
        .export_migrate_state()
        .expect("export")
        .expect("an enabled controller has state")
}

/// Commands the guest queued while the source was paused have no
/// doorbell left to ring on the destination.
#[test]
fn a_restored_controller_runs_what_the_guest_left_queued_on_resume() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    src.ctrl.pause();
    for slot in 0..2u16 {
        put_sqe(
            &src,
            SQ1,
            slot,
            Sqe::read(u64::from(slot), 1, 0x50000, 0),
            slot,
        );
    }
    src.sq_doorbell(1, 2);
    let state = export(&src);

    let manual = Manual::new(IO_QDEPTH);
    let dst = Harness::manual(&manual);
    copy_ram(&src, &dst, SQ1, 2 * SQE_SIZE);
    dst.ctrl.restore_migrate_state(&state).expect("restore");
    assert_eq!(manual.queued(), 0, "a restore started I/O before resume");

    dst.ctrl.resume();
    assert_eq!(manual.queued(), 2, "resume left the commands queued");
    manual.complete_all();
    assert_eq!(dst.wait_cqe(CQ1, 0, true).cid, 0);
    assert_eq!(dst.wait_cqe(CQ1, 1, true).cid, 1);
}

/// The destination must not touch the disk before the source commits,
/// and a doorbell is the one way left to start the submit path.
#[test]
fn a_restored_controller_admits_nothing_before_resume() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let state = export(&src);

    let manual = Manual::new(IO_QDEPTH);
    let dst = Harness::manual(&manual);
    dst.ctrl.restore_migrate_state(&state).expect("restore");
    put_sqe(&dst, SQ1, 0, Sqe::read(0, 1, 0x50000, 0), 0x61);
    dst.sq_doorbell(1, 1);

    assert_eq!(manual.queued(), 0, "I/O ran before the commit");
    assert!(dst.ctrl.is_quiesced());
    dst.ctrl.resume();
    assert_eq!(manual.queued(), 1);
}

fn nvme(state: vmm_devices::DeviceMigrateState) -> NvmeMigrateState {
    match state {
        vmm_devices::DeviceMigrateState::Nvme(state) => state,
        other => panic!("not NVMe state: {other:?}"),
    }
}

fn held(sqid: u16, cid: u16) -> NvmeMigrateCqe {
    NvmeMigrateCqe {
        sqid,
        cid,
        status: 0,
        cdw0: 0,
    }
}

#[test]
fn held_completions_cross_a_migration() {
    let manual = Manual::new(IO_QDEPTH);
    let mut src = Harness::manual(&manual);
    src.enable();
    src.create_io_queues_sized(1, IO_QDEPTH, 2, SQ1, CQ1);
    let mut tail = 0;
    for cid in [0x71, 0x72] {
        submit_on(&src, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    manual.complete_all();
    src.ctrl.pause();
    let state = export(&src);
    let cq = nvme(state.clone()).io_cqs[0].clone().expect("CQ 1");
    assert_eq!(cq.held, vec![held(1, 0x72)]);

    let dst = Harness::new();
    copy_ram(&src, &dst, CQ1, 2 * CQE_SIZE);
    dst.ctrl.restore_migrate_state(&state).expect("restore");
    dst.ctrl.resume();
    assert_eq!(dst.read_guest(CQ1 + CQE_SIZE as u64, CQE_SIZE), vec![0; 16]);

    dst.cq_doorbell(1, 1);
    let cqe = dst.wait_cqe(CQ1, 1, true);
    assert_eq!((cqe.cid, cqe.sqid), (0x72, 1));
    assert_eq!(cqe.status(), Status::SUCCESS);
}

#[test]
fn a_held_completion_for_an_sq_that_posts_elsewhere_is_refused() {
    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").held = vec![held(1, 9)];
    let error = check_nvme_state(&payload).expect_err("SQ 1 posts to CQ 1");
    assert!(error.to_string().contains("does not post"), "{error}");

    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").held = vec![held(2, 9)];
    check_nvme_state(&payload).expect_err("there is no SQ 2");

    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").held = vec![held(1, 9)];
    payload.admin_cq.as_mut().expect("admin cq").held = vec![held(0, 3)];
    check_nvme_state(&payload).expect("both post where they are held");
}

#[test]
fn a_held_status_wider_than_a_cqe_is_refused() {
    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").held = vec![NvmeMigrateCqe {
        status: 0x8000,
        ..held(1, 9)
    }];
    let error = check_nvme_state(&payload).expect_err("status bit 15");
    assert!(error.to_string().contains("does not fit"), "{error}");
}

#[test]
fn more_held_completions_than_the_local_depth_are_refused() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut state = nvme(export(&src));
    state.io_cqs[0].as_mut().expect("CQ 1").held =
        vec![held(1, 1), held(1, 2), held(1, 3)];

    let manual = Manual::new(2);
    let dst = Harness::manual(&manual);
    let error = dst
        .ctrl
        .restore_migrate_state(&vmm_devices::DeviceMigrateState::Nvme(state))
        .expect_err("three held, depth two");
    assert!(error.to_string().contains("depth"), "{error}");
}

/// An I/O SQ posting to CQ 0 would put I/O completions, held ones
/// included, in the admin ring.
#[test]
fn an_io_sq_that_posts_to_the_admin_cq_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").cq_id = 0;
    let error = check_nvme_state(&payload).expect_err("I/O SQ on CQ 0");
    assert!(error.to_string().contains("other kind"), "{error}");

    payload.admin_cq.as_mut().expect("admin cq").held = vec![held(1, 9)];
    check_nvme_state(&payload).expect_err("held I/O CQE in the admin CQ");

    let mut payload = good_payload();
    payload.admin_sq.as_mut().expect("admin sq").cq_id = 1;
    check_nvme_state(&payload).expect_err("admin SQ on an I/O CQ");
}

/// Their completions would be matched against the queues the import
/// replaces.
#[test]
fn a_restore_onto_a_controller_with_commands_in_flight_is_refused() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let state = export(&src);

    let manual = Manual::new(IO_QDEPTH);
    let mut dst = Harness::manual(&manual);
    dst.enable();
    dst.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    submit_on(&dst, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x81);
    assert_eq!(manual.queued(), 1);

    let error = dst
        .ctrl
        .restore_migrate_state(&state)
        .expect_err("a command is in flight");
    assert!(error.to_string().contains("in flight"), "{error}");
}

/// Only a CQ head doorbell writes a held completion otherwise, and a
/// guest waiting on this one has nothing to consume.
#[test]
fn a_held_completion_with_room_in_its_cq_goes_in_on_resume() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut state = nvme(export(&src));
    state.io_cqs[0].as_mut().expect("CQ 1").held = vec![held(1, 0x99)];

    let dst = Harness::new();
    dst.ctrl
        .restore_migrate_state(&vmm_devices::DeviceMigrateState::Nvme(state))
        .expect("restore");
    dst.ctrl.resume();
    let cqe = dst.wait_cqe(CQ1, 0, true);
    assert_eq!((cqe.cid, cqe.sqid), (0x99, 1));
}

/// A controller holds at most its depth per SQ, so the wire bound must
/// take the most any backend depth allows.
#[test]
fn the_most_held_completions_a_controller_can_make_restore() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut state = nvme(export(&src));
    let cq = state.io_cqs[0].as_mut().expect("CQ 1");
    cq.held = (0..MAX_QUEUE_SIZE).map(|cid| held(1, cid)).collect();

    let dst = Harness::manual(&Manual::new(u16::MAX));
    dst.ctrl
        .restore_migrate_state(&vmm_devices::DeviceMigrateState::Nvme(
            state.clone(),
        ))
        .expect("a full queue of held completions");

    state.io_cqs[0]
        .as_mut()
        .expect("CQ 1")
        .held
        .push(held(1, 0));
    check_nvme_state(&state).expect_err("one past the wire bound");
}

#[test]
fn a_refused_msix_table_changes_nothing() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut state = nvme(export(&src));
    state.msix.entries.push(Default::default());

    let dst = Harness::new();
    let error = dst
        .ctrl
        .restore_migrate_state(&vmm_devices::DeviceMigrateState::Nvme(state))
        .expect_err("one entry too many");
    assert!(error.to_string().contains("MSI-X"), "{error}");
    let st = dst.ctrl.lock();
    assert_eq!(st.cc, 0, "the controller registers were applied");
    assert!(st.admin_sq.is_none(), "the admin queue was applied");
    assert!(!st.admission.paused, "admission was paused");
}
